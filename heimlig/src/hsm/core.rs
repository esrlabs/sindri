use crate::client;
use crate::common::jobs;
use crate::common::jobs::{ClientId, Request, RequestType, Response};
use crate::hsm::keystore::KeyStore;
use core::borrow::BorrowMut;
use core::cell::RefCell;
use core::future::{pending, poll_fn, ready};
use core::ops::DerefMut;
use core::pin::Pin;
use core::task::Poll;
use embassy_futures::select::select_array;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::mutex::Mutex;
use futures::future::{join, select, Either};
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt};
use heapless::Vec;

use super::util::join_vec;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Error {
    /// Queue specific error
    Send,
    /// Job specific error
    Job(jobs::Error),
}

/// HSM core that waits for [Request]s from clients and send [Response]s once they are ready.   
pub struct Core<
    'data,
    'keystore,
    M: RawMutex, // TODO: Get rid of embassy specific mutex outside of integration code
    ReqSrc: Stream<Item = Request<'data>>,
    RespSink: Sink<Response<'data>>,
    ReqSink: Sink<Request<'data>>,
    RespSrc: Stream<Item = Response<'data>>,
    const MAX_REQUEST_TYPES: usize = 8,
    const MAX_CLIENTS: usize = 8,
    const MAX_WORKERS: usize = 8,
> {
    key_store: Option<&'keystore Mutex<M, &'keystore mut (dyn KeyStore + Send)>>,
    clients: Vec<RefCell<ClientChannel<'data, ReqSrc, RespSink>>, MAX_CLIENTS>,
    workers: Vec<RefCell<WorkerChannel<'data, ReqSink, RespSrc, MAX_REQUEST_TYPES>>, MAX_WORKERS>,
    last_client_id: usize,
    last_worker_id: usize,
}

struct ClientChannel<'data, ReqSrc: Stream<Item = Request<'data>>, RespSink: Sink<Response<'data>>>
{
    requests: futures::stream::Peekable<ReqSrc>,
    responses: RespSink,
}

/// Associate request types with request sink and response source of the responsible worker
struct WorkerChannel<
    'data,
    ReqSink: Sink<Request<'data>>,
    RespSrc: Stream<Item = Response<'data>>,
    const MAX_REQUEST_TYPES_PER_WORKER: usize,
> {
    pub req_types: Vec<RequestType, MAX_REQUEST_TYPES_PER_WORKER>,
    pub requests: ReqSink,
    pub responses: futures::stream::Peekable<RespSrc>,
}

pub struct Builder<
    'data,
    'keystore,
    M: RawMutex, // TODO: Get rid of embassy specific mutex outside of integration code
    ReqSrc: Stream<Item = Request<'data>>,
    RespSink: Sink<Response<'data>>,
    ReqSink: Sink<Request<'data>>,
    RespSrc: Stream<Item = Response<'data>>,
    const MAX_REQUEST_TYPES: usize = 8,
    const MAX_CLIENTS: usize = 8,
    const MAX_WORKERS: usize = 8,
> {
    key_store: Option<&'keystore Mutex<M, &'keystore mut (dyn KeyStore + Send)>>,
    clients: Vec<RefCell<ClientChannel<'data, ReqSrc, RespSink>>, MAX_CLIENTS>,
    workers: Vec<RefCell<WorkerChannel<'data, ReqSink, RespSrc, MAX_REQUEST_TYPES>>, MAX_WORKERS>,
}

impl<
        'data,
        'keystore,
        M: RawMutex,
        ReqSrc: Stream<Item = Request<'data>> + Unpin,
        RespSink: Sink<Response<'data>> + Unpin,
        ReqSink: Sink<Request<'data>> + Unpin,
        RespSrc: Stream<Item = Response<'data>> + Unpin,
        const MAX_REQUESTS_PER_WORKER: usize,
        const MAX_CLIENTS: usize,
        const MAX_WORKERS: usize,
    > Default
    for Builder<
        'data,
        'keystore,
        M,
        ReqSrc,
        RespSink,
        ReqSink,
        RespSrc,
        MAX_REQUESTS_PER_WORKER,
        MAX_CLIENTS,
        MAX_WORKERS,
    >
{
    fn default() -> Self {
        Builder::new()
    }
}

impl<
        'data,
        'keystore,
        M: RawMutex,
        ReqSrc: Stream<Item = Request<'data>> + Unpin,
        RespSink: Sink<Response<'data>> + Unpin,
        ReqSink: Sink<Request<'data>> + Unpin,
        RespSrc: Stream<Item = Response<'data>> + Unpin,
        const MAX_REQUESTS_PER_WORKER: usize,
        const MAX_CLIENTS: usize,
        const MAX_WORKERS: usize,
    >
    Builder<
        'data,
        'keystore,
        M,
        ReqSrc,
        RespSink,
        ReqSink,
        RespSrc,
        MAX_REQUESTS_PER_WORKER,
        MAX_CLIENTS,
        MAX_WORKERS,
    >
{
    pub fn new() -> Builder<
        'data,
        'keystore,
        M,
        ReqSrc,
        RespSink,
        ReqSink,
        RespSrc,
        MAX_REQUESTS_PER_WORKER,
        MAX_CLIENTS,
        MAX_WORKERS,
    > {
        Builder {
            key_store: None,
            clients: Default::default(),
            workers: Default::default(),
        }
    }

    pub fn with_keystore(
        mut self,
        key_store: &'keystore Mutex<M, &'keystore mut (dyn KeyStore + Send)>,
    ) -> Self {
        self.key_store = Some(key_store);
        self
    }

    pub fn with_client(mut self, requests: ReqSrc, responses: RespSink) -> Self {
        if self
            .clients
            .push(RefCell::new(ClientChannel {
                requests: requests.peekable(),
                responses,
            }))
            .is_err()
        {
            panic!("Failed to add client channel");
        };
        self
    }

    pub fn with_worker(
        mut self,
        req_types: &[RequestType],
        requests: ReqSink,
        responses: RespSrc,
    ) -> Self {
        for channel in &mut self.workers {
            for req_type in req_types {
                if channel.get_mut().req_types.contains(req_type) {
                    panic!("Channel for given request type already exists");
                }
            }
        }
        if self
            .workers
            .push(RefCell::new(WorkerChannel {
                req_types: Vec::from_slice(req_types)
                    .expect("Maximum number of request types for single worker exceeded"),
                requests,
                responses: responses.peekable(),
            }))
            .is_err()
        {
            panic!("Failed to add worker channel");
        };
        self
    }

    pub fn build(
        self,
    ) -> Core<
        'data,
        'keystore,
        M,
        ReqSrc,
        RespSink,
        ReqSink,
        RespSrc,
        MAX_REQUESTS_PER_WORKER,
        MAX_CLIENTS,
        MAX_WORKERS,
    > {
        Core {
            key_store: self.key_store,
            clients: self.clients,
            workers: self.workers,
            last_client_id: 0,
            last_worker_id: 0,
        }
    }
}

impl<
        'data,
        'keystore,
        M: RawMutex,
        ReqSrc: Stream<Item = Request<'data>> + Unpin,
        RespSink: Sink<Response<'data>> + Unpin,
        ReqSink: Sink<Request<'data>> + Unpin,
        RespSrc: Stream<Item = Response<'data>> + Unpin,
        const MAX_REQUESTS_PER_WORKER: usize,
        const MAX_CLIENTS: usize,
        const MAX_WORKERS: usize,
    >
    Core<
        'data,
        'keystore,
        M,
        ReqSrc,
        RespSink,
        ReqSink,
        RespSrc,
        MAX_REQUESTS_PER_WORKER,
        MAX_CLIENTS,
        MAX_WORKERS,
    >
{
    pub async fn execute(&mut self) -> Result<(), Error> {
        self.process_client_requests().await;
        // self.process_worker_responses()?;
        Ok(())
    }

    /// Search all input channels for a new request and process it.
    /// Channels are processed in a round-robin fashion.
    async fn process_client_requests(&mut self) {
        let number_of_clients = self.clients.len();
        let (left, right) = self
            .clients
            .split_at_mut((self.last_client_id + 1) % number_of_clients);
        let clients_iterator = right.into_iter().chain(left.into_iter());

        let mut client_refs =
            Vec::<_, MAX_CLIENTS>::from_iter(clients_iterator.map(|client| (*client).borrow_mut()));

        let mut client_futures =
            Vec::<_, MAX_CLIENTS>::from_iter(client_refs.iter_mut().map(|client| {
                let requests = Pin::new(&mut client.requests);
                requests
                    .peek()
                    .then(|request| {
                        let request_type = request.expect("requests stream died").get_type();
                        let worker_channel = self
                            .workers
                            .iter()
                            .find(|c| {
                                c.try_borrow()
                                    .expect("futures are expected to be polled sequentially")
                                    .req_types
                                    .contains(&request_type)
                            })
                            .expect("Failed to find worker channel for request type");
                        poll_fn(move |cx| {
                            worker_channel
                                .try_borrow_mut()
                                .expect("futures are expected to be polled sequentially")
                                .requests
                                .poll_ready_unpin(cx)
                                .map(|_| (worker_channel))
                        })
                    })
                    .left_future()
            }));
        for _ in client_futures.len()..client_futures.capacity() {
            unsafe { client_futures.push_unchecked(pending().right_future()) };
        }

        let (worker_channel, client_index) = select_array(
            client_futures
                .into_array::<MAX_CLIENTS>()
                .map_err(|_| ())
                .expect("vec was extended up to capacity"),
        )
        .await;

        drop(client_refs);

        assert!(client_index < number_of_clients);
        self.last_client_id = (client_index + self.last_client_id + 1) % number_of_clients;
        let request = self.clients[client_index]
            .borrow_mut()
            .requests
            .next()
            .await
            .expect("request stream died");
        worker_channel
            .borrow_mut()
            .requests
            .send(request)
            .await
            .map_err(|_| ())
            .expect("request sink died");
    }

    async fn process_worker_responses(&mut self) -> Result<(), Error> {
        // let number_of_workers = self.workers.len();
        // let (left, right) = self
        //     .workers
        //     .split_at_mut((self.last_worker_id + 1) % number_of_workers);
        // let mut workers_iterator = right.into_iter().chain(left.into_iter());

        // let workers: [_; MAX_WORKERS] = core::array::from_fn(|_| {
        //     if let Some(&mut worker) = workers_iterator.next() {
        //         let worker_responses = Pin::new(&mut worker.borrow_mut().responses);
        //         worker_responses
        //             .peek()
        //             .then(|response| {
        //                 // let request_type = request.expect("requests stream died").get_type();
        //                 let client_channel =
        //                     self.clients
        //                         .get_mut(response.expect("response stream died").get_client_id()
        //                             as usize)
        //                         .expect("Invalid internal client ID");
        //                 poll_fn(move |cx| {
        //                     client_channel
        //                         .try_borrow_mut()
        //                         .expect("futures are expected to be polled sequentially")
        //                         .responses
        //                         .poll_ready_unpin(cx)
        //                         .map(|_| (client_channel))
        //                 })
        //             })
        //             .left_future()
        //     } else {
        //         pending().right_future()
        //     }
        // });

        // let (worker_channel, client_index) = select_array(workers).await;
        // assert!(client_index < self.clients.len());
        // self.last_client_id = (client_index + self.last_client_id + 1) % number_of_clients;
        // let request = self.clients[client_index]
        //     .requests
        //     .next()
        //     .await
        //     .expect("request stream died");
        // worker_channel
        //     .borrow_mut()
        //     .requests
        //     .send(request)
        //     .await
        //     .map_err(|_| ())
        //     .expect("request sink died");

        // let workers_len = self.workers.len();
        // for worker_index in 0..workers_len {
        //     let worker = self.workers.get_mut(worker_index);
        //     if let Some(worker) = worker {
        //         let response = worker.get_mut().responses.next().await;
        //         if let Some(response) = response {
        //             self.send_to_client(response).await?;
        //         }
        //     } else {
        //         panic!("Invalid internal worker ID");
        //     }
        // }
        Ok(()) // Nothing to process
    }

    async fn process_request(&mut self, request: Request<'data>) -> Result<(), Error> {
        match request {
            Request::ImportKey {
                client_id,
                request_id,
                key_id,
                data,
            } => {
                let response = {
                    if let Some(key_store) = self.key_store {
                        match key_store
                            .try_lock()
                            .expect("Failed to lock key store")
                            .deref_mut()
                            .import(key_id, data)
                        {
                            Ok(()) => Response::ImportKey {
                                client_id,
                                request_id,
                            },
                            Err(e) => Response::Error {
                                client_id,
                                request_id,
                                error: jobs::Error::KeyStore(e),
                            },
                        }
                    } else {
                        Response::Error {
                            client_id,
                            request_id,
                            error: jobs::Error::NoKeyStore,
                        }
                    }
                };
                self.send_to_client(response).await?;
            }
            _ => {
                let channel = self
                    .workers
                    .iter_mut()
                    .find(|c| c.borrow().req_types.contains(&request.get_type()))
                    .expect("Failed to find worker channel for request type");
                channel
                    .get_mut()
                    .requests
                    .send(request)
                    .await
                    .map_err(|_e| Error::Send)?;
            }
        }
        Ok(())
    }

    async fn send_to_client(&mut self, response: Response<'data>) -> Result<(), Error> {
        let client_id = response.get_client_id();
        if let Some(client) = self.clients.get(client_id as usize) {
            client
                .borrow_mut()
                .responses
                .send(response)
                .await
                .map_err(|_e| Error::Send)?;
        } else {
            panic!("Invalid internal client ID");
        }
        Ok(())
    }
}
