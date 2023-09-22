#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use defmt::*;
use embassy_executor::Spawner;
use embassy_stm32::bind_interrupts;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::peripherals::RNG;
use embassy_stm32::rng::{InterruptHandler, Rng};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Timer};
use heapless::spsc::{Consumer, Producer, Queue};
use heimlig::client::api::Api;
use heimlig::common::jobs::{Request, RequestType, Response};
use heimlig::crypto::rng;
use heimlig::hsm::core::{Builder, Core};
use heimlig::hsm::workers::rng_worker::RngWorker;
use heimlig::integration::embassy::{
    RequestQueueSink, RequestQueueSource, ResponseQueueSink, ResponseQueueSource,
};
use rand_core::RngCore;

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    RNG => InterruptHandler<RNG>;
});

// Shared memory pool
static mut MEMORY: [u8; 256] = [0; 256];

const QUEUE_SIZE: usize = 8;
static mut CLIENT_TO_CORE: Queue<Request, QUEUE_SIZE> = Queue::new();
static mut CORE_TO_CLIENT: Queue<Response, QUEUE_SIZE> = Queue::new();
static mut CORE_TO_RNG_WORKER: Queue<Response, QUEUE_SIZE> = Queue::new();
static mut RNG_WORKER_TO_CORE: Queue<Request, QUEUE_SIZE> = Queue::new();

struct EntropySource {
    rng: Rng<'static, RNG>,
}

impl rng::EntropySource for EntropySource {
    fn random_seed(&mut self) -> [u8; 32] {
        let mut buf = [0u8; 32];
        self.rng.fill_bytes(&mut buf);
        info!("New random seed (size={}, data={:02x})", buf.len(), buf);
        buf
    }
}

#[embassy_executor::task]
async fn hsm_task(
    client_req_rx: Consumer<'static, Request<'_>, QUEUE_SIZE>,
    client_resp_tx: Producer<'static, Response<'_>, QUEUE_SIZE>,
    rng_req_tx: Producer<'static, Request<'_>, QUEUE_SIZE>,
    rng_req_rx: Consumer<'static, Request<'_>, QUEUE_SIZE>,
    rng_resp_tx: Producer<'static, Response<'_>, QUEUE_SIZE>,
    rng_resp_rx: Consumer<'static, Response<'_>, QUEUE_SIZE>,
    rng: Rng<'static, RNG>,
) {
    info!("HSM task started");

    // Channels
    let client_requests: RequestQueueSource<NoopRawMutex, QUEUE_SIZE> =
        RequestQueueSource::new(client_req_rx);
    let client_responses: ResponseQueueSink<NoopRawMutex, QUEUE_SIZE> =
        ResponseQueueSink::new(client_resp_tx);
    let rng_requests_rx: RequestQueueSource<NoopRawMutex, QUEUE_SIZE> =
        RequestQueueSource::new(rng_req_rx);
    let rng_requests_tx: RequestQueueSink<NoopRawMutex, QUEUE_SIZE> =
        RequestQueueSink::new(rng_req_tx);
    let rng_responses_rx: ResponseQueueSource<NoopRawMutex, QUEUE_SIZE> =
        ResponseQueueSource::new(rng_resp_rx);
    let rng_responses_tx: ResponseQueueSink<NoopRawMutex, QUEUE_SIZE> =
        ResponseQueueSink::new(rng_resp_tx);

    let rng = rng::Rng::new(EntropySource { rng }, None);
    let mut rng_worker = RngWorker {
        rng,
        requests: rng_requests_rx,
        responses: rng_responses_tx,
    };
    let mut core: Core<
        NoopRawMutex,
        RequestQueueSource<'_, '_, NoopRawMutex, QUEUE_SIZE>,
        ResponseQueueSink<'_, '_, NoopRawMutex, QUEUE_SIZE>,
        RequestQueueSink<'_, '_, NoopRawMutex, QUEUE_SIZE>,
        ResponseQueueSource<'_, '_, NoopRawMutex, QUEUE_SIZE>,
    > = Builder::new()
        .with_client(client_requests, client_responses)
        .with_worker(&[RequestType::GetRandom], rng_requests_tx, rng_responses_rx)
        .build();

    loop {
        core.execute().await.expect("failed to forward request");
        rng_worker
            .execute()
            .await
            .expect("failed to process request");
        Timer::after(Duration::from_millis(100)).await;
    }
}

#[embassy_executor::task]
async fn client_task(
    resp_rx: Consumer<'static, Response<'_>, QUEUE_SIZE>,
    req_tx: Producer<'static, Request<'_>, QUEUE_SIZE>,
    mut led: Output<'static, embassy_stm32::peripherals::PJ2>,
) {
    info!("Client task started");

    // Memory
    let pool = heapless::pool::Pool::<[u8; 16]>::new();
    // Safety: we are the only users of MEMORY
    pool.grow(unsafe { &mut MEMORY });

    // Channel
    let requests: RequestQueueSink<NoopRawMutex, QUEUE_SIZE> = RequestQueueSink::new(req_tx);
    let responses: ResponseQueueSource<NoopRawMutex, QUEUE_SIZE> =
        ResponseQueueSource::new(resp_rx);

    // Api
    let mut api = Api::new(requests, responses);

    loop {
        // Send requests
        Timer::after(Duration::from_millis(1000)).await;
        led.set_high();

        let mut random_buffer_alloc = pool
            .alloc()
            .expect("Failed to allocate buffer for random data")
            .init([0; 16]);
        // Safety: we forget about the box below, so it doesn't get dropped!
        let random_buffer = unsafe {
            core::slice::from_raw_parts_mut(
                random_buffer_alloc.as_mut_ptr(),
                random_buffer_alloc.len(),
            )
        };
        // Avoid releasing the allocation; unfortunately with current version of heapless, we
        // cannot unleak this. heapless::pool::Box would need to implement an interface similar to
        // std::Box::from_raw.
        core::mem::forget(random_buffer_alloc);
        let request_size = random_buffer.len();
        let request_id = api
            .get_random(random_buffer)
            .await
            .expect("failed to call randomness API");
        info!(
            "--> request:  random data (id={}) (size={})",
            request_id, request_size
        );

        // Receive response
        loop {
            if let Some(response) = api.recv_response().await {
                match response {
                    Response::GetRandom {
                        client_id: _,
                        request_id,
                        data,
                    } => {
                        info!(
                            "<-- response: random data (id={}) (size={}): {}",
                            request_id,
                            data.len(),
                            data
                        );
                        break;
                    }
                    _ => error!("Unexpected response type"),
                }
            }
            Timer::after(Duration::from_millis(100)).await;
            led.set_low();
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Main task started");

    // Random number generator
    let peripherals = embassy_stm32::init(Default::default());
    let rng = Rng::new(peripherals.RNG, Irqs);
    let led = Output::new(peripherals.PJ2, Level::High, Speed::Low);

    // Queues
    // Unsafe: Access to mutable static only happens here. Static lifetime is required by embassy tasks.
    let (client_req_tx, client_req_rx) = unsafe { CLIENT_TO_CORE.split() };
    let (client_resp_tx, client_resp_rx) = unsafe { CORE_TO_CLIENT.split() };
    let (rng_resp_tx, rng_resp_rx) = unsafe { CORE_TO_RNG_WORKER.split() };
    let (rng_req_tx, rng_req_rx) = unsafe { RNG_WORKER_TO_CORE.split() };

    // Start tasks
    spawner
        .spawn(hsm_task(
            client_req_rx,
            client_resp_tx,
            rng_req_tx,
            rng_req_rx,
            rng_resp_tx,
            rng_resp_rx,
            rng,
        ))
        .expect("Failed to spawn HSM task");
    spawner
        .spawn(client_task(client_resp_rx, client_req_tx, led))
        .expect("Failed to spawn client task");
}
