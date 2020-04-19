#[macro_use]
pub mod proto;

use crate::tcp::client::Client;
use crate::utils::mutex::Mutex;
use crate::utils::rwlock::RwLock;
use crate::{tcp, DISABLE_SHORTCUT};
use bifrost_hasher::hash_str;
use byteorder::{ByteOrder, LittleEndian};
use bytes::buf::BufExt;
use bytes::{Buf, BufMut, BytesMut};
use futures::future::{err, BoxFuture};
use futures::prelude::*;
use futures::{future, Future};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::time::delay_for;

lazy_static! {
    pub static ref DEFAULT_CLIENT_POOL: ClientPool = ClientPool::new();
}

#[derive(Serialize, Deserialize, Debug)]
pub enum RPCRequestError {
    FunctionIdNotFound,
    ServiceIdNotFound,
    Other,
}

#[derive(Debug)]
pub enum RPCError {
    IOError(io::Error),
    RequestError(RPCRequestError),
}

pub trait RPCService: Sync + Send {
    fn dispatch(&self, data: BytesMut) -> BoxFuture<Result<BytesMut, RPCRequestError>>;
    fn register_shortcut_service(
        &self,
        service_ptr: usize,
        server_id: u64,
        service_id: u64,
    ) -> ::std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;
}

pub struct Server {
    services: RwLock<HashMap<u64, Arc<dyn RPCService>>>,
    pub address: String,
    pub server_id: u64,
}

unsafe impl Sync for Server {}

pub struct ClientPool {
    clients: Arc<Mutex<HashMap<u64, Arc<RPCClient>>>>,
}

fn encode_res(res: Result<BytesMut, RPCRequestError>) -> BytesMut {
    match res {
        Ok(buffer) => [0u8; 1].iter().cloned().chain(buffer.into_iter()).collect(),
        Err(e) => {
            let err_id = match e {
                RPCRequestError::FunctionIdNotFound => 1u8,
                RPCRequestError::ServiceIdNotFound => 2u8,
                _ => 255u8,
            };
            BytesMut::from(&[err_id][..])
        }
    }
}

fn decode_res(res: io::Result<BytesMut>) -> Result<BytesMut, RPCError> {
    match res {
        Ok(mut res) => {
            if res[0] == 0u8 {
                res.advance(1);
                Ok(res)
            } else {
                match res[0] {
                    1u8 => Err(RPCError::RequestError(RPCRequestError::FunctionIdNotFound)),
                    2u8 => Err(RPCError::RequestError(RPCRequestError::ServiceIdNotFound)),
                    _ => Err(RPCError::RequestError(RPCRequestError::Other)),
                }
            }
        }
        Err(e) => Err(RPCError::IOError(e)),
    }
}

pub fn read_u64_head(mut data: BytesMut) -> (u64, BytesMut) {
    let num = data.get_u64_le();
    LittleEndian::read_u64(data.as_ref());
    (num, data)
}

impl Server {
    pub fn new(address: &String) -> Arc<Server> {
        Arc::new(Server {
            services: RwLock::new(HashMap::new()),
            address: address.clone(),
            server_id: hash_str(address),
        })
    }
    pub async fn listen(server: &Arc<Server>) -> Result<(), Box<dyn Error>> {
        let address = &server.address;
        let server = server.clone();
        tcp::server::Server::new(
            address,
            Arc::new(move |mut data| {
                let server = server.clone();
                async move {
                    let (svr_id, data) = read_u64_head(data);
                    let svr_map = server.services.read().await;
                    let service = svr_map.get(&svr_id);
                    match service {
                        Some(ref service) => {
                            let svr_res = service.dispatch(data).await;
                            encode_res(svr_res)
                        }
                        None => {
                            let svr_ids = svr_map.keys().collect::<Vec<_>>();
                            debug!("Service Id NOT found {}, have {:?}", svr_id, svr_ids);
                            encode_res(Err(RPCRequestError::ServiceIdNotFound))
                        }
                    }
                }
                .boxed()
            }),
        )
        .await
    }

    pub async fn listen_and_resume(server: &Arc<Server>) {
        let server = server.clone();
        tokio::spawn(async move {
            Self::listen(&server).await.unwrap();
        });
        delay_for(Duration::from_secs(1)).await
    }

    pub async fn register_service<T>(&self, service_id: u64, service: &Arc<T>)
    where
        T: RPCService + Sized + 'static,
    {
        let service = service.clone();
        if !DISABLE_SHORTCUT {
            let service_ptr = Arc::into_raw(service.clone()) as usize;
            service
                .register_shortcut_service(service_ptr, self.server_id, service_id)
                .await;
        } else {
            println!("SERVICE SHORTCUT DISABLED");
        }
        self.services.write().await.insert(service_id, service);
    }

    pub async fn remove_service(&self, service_id: u64) {
        self.services.write().await.remove(&service_id);
    }
    pub fn address(&self) -> &String {
        &self.address
    }
}

pub struct RPCClient {
    client: Mutex<tcp::client::Client>,
    pub server_id: u64,
    pub address: String,
}

pub fn prepend_u64(num: u64, data: BytesMut) -> BytesMut {
    let mut bytes = BytesMut::with_capacity(8);
    bytes.put_u64_le(num);
    bytes.unsplit(data);
    bytes
}

impl RPCClient {
    pub async fn send_async(
        self: Pin<&Self>,
        svr_id: u64,
        data: BytesMut,
    ) -> Result<BytesMut, RPCError> {
        let mut client = self.client.lock().await;
        let bytes = prepend_u64(svr_id, data);
        decode_res(Client::send_msg(Pin::new(&mut *client), bytes).await)
    }
    pub async fn new_async(addr: &String) -> io::Result<Arc<RPCClient>> {
        let client = tcp::client::Client::connect(addr).await?;
        Ok(Arc::new(RPCClient {
            server_id: client.server_id,
            client: Mutex::new(client),
            address: addr.clone(),
        }))
    }
}

impl ClientPool {
    pub fn new() -> ClientPool {
        ClientPool {
            clients: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn get(&self, addr: &String) -> io::Result<Arc<RPCClient>> {
        let addr_clone = addr.clone();
        let server_id = hash_str(addr);
        self.get_by_id(server_id, move |_| addr_clone).await
    }

    pub async fn get_by_id<F>(&self, server_id: u64, addr_fn: F) -> io::Result<Arc<RPCClient>>
    where
        F: FnOnce(u64) -> String,
    {
        let mut clients = self.clients.lock().await;
        if clients.contains_key(&server_id) {
            let client = clients.get(&server_id).unwrap().clone();
            Ok(client)
        } else {
            let mut client = RPCClient::new_async(&addr_fn(server_id)).await?;
            clients.insert(server_id, client.clone());
            Ok(client)
        }
    }
}

#[cfg(test)]
mod test {
    use futures::future::BoxFuture;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::delay_for;

    pub mod simple_service {

        use super::*;

        service! {
            rpc hello(name: String) -> String;
            rpc error(message: String) -> Result<(), String>;
        }

        struct HelloServer;

        impl Service for HelloServer {
            fn hello(&self, name: String) -> BoxFuture<String> {
                future::ready(format!("Hello, {}!", name)).boxed()
            }
            fn error(&self, message: String) -> BoxFuture<Result<(), String>> {
                future::ready(Err(message.clone())).boxed()
            }
        }
        dispatch_rpc_service_functions!(HelloServer);

        #[tokio::test(threaded_scheduler)]
        pub async fn simple_rpc() {
            let addr = String::from("127.0.0.1:1300");
            {
                let addr = addr.clone();
                let server = Server::new(&addr);
                server.register_service(0, &Arc::new(HelloServer)).await;
                Server::listen_and_resume(&server);
            }
            delay_for(Duration::from_millis(1000)).await;
            let client = RPCClient::new_async(&addr).await.unwrap();
            let service_client = AsyncServiceClient::new(0, &client);
            let response = service_client.hello(String::from("Jack")).await;
            let greeting_str = response.unwrap();
            println!("SERVER RESPONDED: {}", greeting_str);
            assert_eq!(greeting_str, String::from("Hello, Jack!"));
            let expected_err_msg = String::from("This error is a good one");
            let response = service_client.error(expected_err_msg.clone());
            let error_msg = response.await.unwrap().err().unwrap();
            assert_eq!(error_msg, expected_err_msg);
        }
    }

    pub mod struct_service {
        use super::*;
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, Debug, Clone)]
        pub struct Greeting {
            pub name: String,
            pub time: u32,
        }

        #[derive(Serialize, Deserialize, Debug)]
        pub struct Respond {
            pub text: String,
            pub owner: u32,
        }

        service! {
            rpc hello(gret: Greeting) -> Respond;
        }

        pub struct HelloServer;

        impl Service for HelloServer {
            fn hello(&self, gret: Greeting) -> BoxFuture<Respond> {
                future::ready(Respond {
                    text: format!("Hello, {}. It is {} now!", gret.name, gret.time),
                    owner: 42,
                })
                .boxed()
            }
        }
        dispatch_rpc_service_functions!(HelloServer);

        #[tokio::test(threaded_scheduler)]
        pub async fn struct_rpc() {
            env_logger::try_init();
            let addr = String::from("127.0.0.1:1400");
            {
                let addr = addr.clone();
                let server = Server::new(&addr); // 0 is service id
                server.register_service(0, &Arc::new(HelloServer)).await;
                Server::listen_and_resume(&server);
            }
            delay_for(Duration::from_millis(1000)).await;
            let client = RPCClient::new_async(&addr).await.unwrap();
            let service_client = AsyncServiceClient::new(0, &client);
            let response = service_client.hello(Greeting {
                name: String::from("Jack"),
                time: 12,
            });
            let res = response.await.unwrap();
            let greeting_str = res.text;
            println!("SERVER RESPONDED: {}", greeting_str);
            assert_eq!(greeting_str, String::from("Hello, Jack. It is 12 now!"));
            assert_eq!(42, res.owner);
        }
    }

    mod multi_server {

        use super::*;

        service! {
            rpc query_server_id() -> u64;
        }

        struct IdServer {
            id: u64,
        }
        impl Service for IdServer {
            fn query_server_id(&self) -> BoxFuture<u64> {
                future::ready(self.id).boxed()
            }
        }
        dispatch_rpc_service_functions!(IdServer);

        #[tokio::test(threaded_scheduler)]
        async fn multi_server_rpc() {
            let addrs = vec![
                String::from("127.0.0.1:1500"),
                String::from("127.0.0.1:1600"),
                String::from("127.0.0.1:1700"),
                String::from("127.0.0.1:1800"),
            ];
            let mut id = 0;
            for addr in &addrs {
                {
                    let addr = addr.clone();
                    let server = Server::new(&addr); // 0 is service id
                    server
                        .register_service(id, &Arc::new(IdServer { id: id }))
                        .await;
                    Server::listen_and_resume(&server);
                    id += 1;
                }
            }
            id = 0;
            delay_for(Duration::from_millis(1000)).await;
            for addr in &addrs {
                let client = RPCClient::new_async(addr).await.unwrap();
                let service_client = AsyncServiceClient::new(id, &client);
                let id_res = service_client.query_server_id().await;
                let id_un = id_res.unwrap();
                assert_eq!(id_un, id);
                id += 1;
            }
        }
    }

    mod parallel {
        use super::struct_service::*;
        use super::*;
        use crate::rpc::{RPCClient, Server, DEFAULT_CLIENT_POOL};
        use bifrost_hasher::hash_str;
        use futures::prelude::stream::*;
        use futures::FutureExt;

        #[tokio::test(threaded_scheduler)]
        pub async fn lots_of_reqs() {
            let addr = String::from("127.0.0.1:1411");
            {
                let addr = addr.clone();
                let server = Server::new(&addr); // 0 is service id
                server.register_service(0, &Arc::new(HelloServer)).await;
                Server::listen_and_resume(&server);
            }
            delay_for(Duration::from_millis(1000)).await;
            let client = RPCClient::new_async(&addr).await.unwrap();
            let service_client = AsyncServiceClient::new(0, &client);

            println!("Testing parallel RPC reqs");

            let mut futs = (0..100)
                .map(|i| {
                    let service_client = service_client.clone();
                    tokio::spawn(async move {
                        let response = service_client.hello(Greeting {
                            name: String::from("John"),
                            time: i,
                        });
                        let res = response.await.unwrap();
                        let greeting_str = res.text;
                        println!("SERVER RESPONDED: {}", greeting_str);
                        assert_eq!(greeting_str, format!("Hello, John. It is {} now!", i));
                        assert_eq!(42, res.owner);
                    })
                    .boxed()
                })
                .collect::<FuturesUnordered<_>>();
            while futs.next().await.is_some() {}

            // test pool
            let server_id = hash_str(&addr);
            let mut futs = (0..100)
                .map(|i| {
                    let addr = (&addr).clone();
                    tokio::spawn(async move {
                        let client = DEFAULT_CLIENT_POOL
                            .get_by_id(server_id, move |_| addr)
                            .await
                            .unwrap();
                        let service_client = AsyncServiceClient::new(0, &client);
                        let response = service_client.hello(Greeting {
                            name: String::from("John"),
                            time: i,
                        });
                        let res = response.await.unwrap();
                        let greeting_str = res.text;
                        println!("SERVER RESPONDED: {}", greeting_str);
                        assert_eq!(greeting_str, format!("Hello, John. It is {} now!", i));
                        assert_eq!(42, res.owner);
                    })
                    .boxed()
                })
                .collect::<FuturesUnordered<_>>();
            while futs.next().await.is_some() {}
        }
    }
}
