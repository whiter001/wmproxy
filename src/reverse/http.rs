use std::{
    collections::{HashSet, HashMap},
    fs::File,
    io::{self, BufReader},
    net::SocketAddr,
    sync::Arc,
};

use crate::{Helper, ProxyResult};
use rustls::{
    server::ResolvesServerCertUsingSni,
    sign::{self, CertifiedKey},
    Certificate, PrivateKey,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::mpsc::{Receiver, Sender},
    sync::Mutex,
};
use tokio_rustls::TlsAcceptor;
use webparse::{Request, Response};
use wenmeng::{ProtError, ProtResult, RecvStream, Server};

use super::{ServerConfig, UpstreamConfig, LocationConfig};

struct InnerHttpOper {
    pub http: Arc<Mutex<HttpConfig>>,
    pub cache_sender: HashMap<LocationConfig, (Sender<Request<RecvStream>>, Receiver<Response<RecvStream>>)>
}

impl InnerHttpOper {
    pub fn new(http: Arc<Mutex<HttpConfig>>) -> Self {
        Self {
            http,
            cache_sender: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "Vec::new")]
    pub server: Vec<ServerConfig>,
    #[serde(default = "Vec::new")]
    pub upstream: Vec<UpstreamConfig>,
}

impl HttpConfig {
    pub fn new() -> Self {
        HttpConfig {
            server: vec![],
            upstream: vec![],
        }
    }

    /// 将配置参数提前共享给子级
    pub fn copy_to_child(&mut self) {
        for server in &mut self.server {
            server.upstream.append(&mut self.upstream.clone());
            server.copy_to_child();
        }
    }

    fn load_certs(path: &Option<String>) -> io::Result<Vec<Certificate>> {
        if let Some(path) = path {
            match File::open(&path) {
                Ok(file) => {
                    let mut reader = BufReader::new(file);
                    let certs = rustls_pemfile::certs(&mut reader)?;
                    Ok(certs.into_iter().map(Certificate).collect())
                }
                Err(e) => {
                    log::warn!("加载公钥{}出错，错误内容:{:?}", path, e);
                    return Err(e);
                }
            }
        } else {
            Err(io::Error::new(io::ErrorKind::Other, "unknow certs"))
        }
    }

    fn load_keys(path: &Option<String>) -> io::Result<PrivateKey> {
        let mut keys = if let Some(path) = path {
            match File::open(&path) {
                Ok(file) => {
                    let mut reader = BufReader::new(file);
                    rustls_pemfile::rsa_private_keys(&mut reader)?
                }
                Err(e) => {
                    log::warn!("加载私钥{}出错，错误内容:{:?}", path, e);
                    return Err(e);
                }
            }
        } else {
            return Err(io::Error::new(io::ErrorKind::Other, "unknow keys"));
        };

        match keys.len() {
            0 => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("No RSA private key found"),
            )),
            1 => Ok(PrivateKey(keys.remove(0))),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("More than one RSA private key found"),
            )),
        }
    }

    pub async fn bind(
        &mut self,
    ) -> ProxyResult<(Option<TlsAcceptor>, Vec<bool>, Vec<TcpListener>)> {
        let mut listeners = vec![];
        let mut tlss = vec![];
        let mut bind_port = HashSet::new();
        let config = rustls::ServerConfig::builder().with_safe_defaults();
        let mut resolve = ResolvesServerCertUsingSni::new();
        for value in &self.server.clone() {
            let mut is_ssl = false;
            if value.cert.is_some() && value.key.is_some() {
                let key = sign::any_supported_type(&Self::load_keys(&value.key)?)
                    .map_err(|_| ProtError::Extension("unvaild key"))?;
                let ck = CertifiedKey::new(Self::load_certs(&value.cert)?, key);
                resolve.add(&value.server_name, ck).map_err(|e| {
                    log::warn!("添加证书时失败:{:?}", e);
                    ProtError::Extension("key error")
                })?;
                is_ssl = true;
            }

            if bind_port.contains(&value.bind_addr.port()) {
                continue;
            }
            bind_port.insert(value.bind_addr.port());
            let listener = Helper::bind(value.bind_addr).await?;
            listeners.push(listener);
            tlss.push(is_ssl);
        }

        let mut config = config
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolve));
        config.alpn_protocols.push("h2".as_bytes().to_vec());
        config.alpn_protocols.push("http/1.1".as_bytes().to_vec());
        Ok((Some(TlsAcceptor::from(Arc::new(config))), tlss, listeners))
    }

    // async fn inner_http_request(
    //     http: &HttpConfig,
    //     req: Request<RecvStream>,
    // ) -> ProtResult<(
    //     Response<RecvStream>,
    //     Option<Sender<Request<RecvStream>>>,
    //     Option<Receiver<Response<RecvStream>>>,
    // )> {
    //     let http = value.http.lock().await;
    //     let server_len = http.server.len();
    //     let host = req.get_host().unwrap_or(String::new());
    //     // 不管有没有匹配, 都执行最后一个
    //     for (index, s) in http.server.iter().enumerate() {
    //         if s.server_name == host || host.is_empty() || index == server_len - 1 {
    //             let path = req.path().clone();
    //             for l in s.location.iter() {
    //                 if l.is_match_rule(&path, req.method()) {
    //                     let (res, sender, receiver) = l.deal_request(req).await?;
    //                     value.sender = sender;
    //                     value.receiver = receiver;
    //                     return Ok(res);
    //                 }
    //             }
    //             return Ok(Response::builder()
    //                 .status(503)
    //                 .body("unknow location to deal")
    //                 .unwrap()
    //                 .into_type());
    //         }
    //     }
    //     return Ok(Response::builder()
    //         .status(503)
    //         .body("unknow location")
    //         .unwrap()
    //         .into_type());
    // }
    
    async fn inner_operate_by_http(mut req: Request<RecvStream>, cache: &mut HashMap<LocationConfig, (Sender<Request<RecvStream>>, Receiver<Response<RecvStream>>)>, http: Arc<Mutex<HttpConfig>> ) -> ProtResult<Response<RecvStream>> {

        let http = http.lock().await;
        let server_len = http.server.len();
        let host = req.get_host().unwrap_or(String::new());
        // 不管有没有匹配, 都执行最后一个
        for (index, s) in http.server.iter().enumerate() {
            if s.server_name == host || host.is_empty() || index == server_len - 1 {
                let path = req.path().clone();
                for l in s.location.iter() {
                    if l.is_match_rule(&path, req.method()) {
                        let clone = l.clone_only_hash();
                        if cache.contains_key(&clone) {
                            let mut cache_client = cache.remove(&clone).unwrap();
                            if !cache_client.0.is_closed() {
                                let send = cache_client.0.send(req).await;
                                println!("send request = {:?}", send);
                                match cache_client.1.recv().await {
                                    Some(res) => {
                                        println!("cache client receive  response");
                                        cache.insert(clone, cache_client);
                                        return Ok(res);
                                    }
                                    None => {
                                        cache.insert(clone, cache_client);
                                        println!("cache client close response");
                                        return Ok(Response::builder()
                                        .status(503)
                                        .body("already lose connection")
                                        .unwrap()
                                        .into_type());
                                    }
                                }
                            }
                        }
                        let (res, sender, receiver) = l.deal_request(req).await?;
                        cache.insert(clone, (sender.unwrap(), receiver.unwrap()));

                        // value.cache_sender[clone] = (sender.unwrap(), receiver.unwrap());
                        // value.cache_sender.insert(clone, (sender.unwrap(), receiver.unwrap()));
                        // value.sender = sender;
                        // value.receiver = receiver;
                        return Ok(res);
                    }
                }
                return Ok(Response::builder()
                    .status(503)
                    .body("unknow location to deal")
                    .unwrap()
                    .into_type());
            }
        }
        return Ok(Response::builder()
            .status(503)
            .body("unknow location")
            .unwrap()
            .into_type());
    }

    async fn inner_operate(mut req: Request<RecvStream>) -> ProtResult<Response<RecvStream>> {
        let data = req.extensions_mut().remove::<Arc<Mutex<InnerHttpOper>>>();
        if data.is_none() {
            return Err(ProtError::Extension("unknow data"));
        }
        let data = data.unwrap();
        let mut value = data.lock().await;
        let http = value.http.clone();
        // let v = {
        //     let http = value.http.lock().await;
        //     Self::inner_http_request(&http, req).await
        // };
        // let http = value.http.clone().lock().await;

        return Self::inner_operate_by_http(req, &mut value.cache_sender, http).await;
        // let server_len = http.server.len();
        // let host = req.get_host().unwrap_or(String::new());
        // // 不管有没有匹配, 都执行最后一个
        // for (index, s) in http.server.iter().enumerate() {
        //     if s.server_name == host || host.is_empty() || index == server_len - 1 {
        //         let path = req.path().clone();
        //         for l in s.location.iter() {
        //             if l.is_match_rule(&path, req.method()) {
        //                 let clone = l.clone_only_hash();
        //                 if value.cache_sender.contains_key(&clone) {
        //                     let mut cache = value.cache_sender.remove(&clone).unwrap();
        //                     if !cache.0.is_closed() {
        //                         cache.0.send(req).await;
        //                         match cache.1.recv().await {
        //                             Some(res) => {
        //                                 value.cache_sender.insert(clone, cache);
        //                                 return Ok(res);
        //                             }
        //                             None => {
        //                                 return Ok(Response::builder()
        //                                 .status(503)
        //                                 .body("already lose connection")
        //                                 .unwrap()
        //                                 .into_type());
        //                             }
        //                         }
        //                     }
        //                 }
        //                 let (res, sender, receiver) = l.deal_request(req).await?;
        //                 drop(http);
        //                 value.cache_sender.insert(clone, (sender.unwrap(), receiver.unwrap()));

        //                 // value.cache_sender[clone] = (sender.unwrap(), receiver.unwrap());
        //                 // value.cache_sender.insert(clone, (sender.unwrap(), receiver.unwrap()));
        //                 // value.sender = sender;
        //                 // value.receiver = receiver;
        //                 return Ok(res);
        //             }
        //         }
        //         return Ok(Response::builder()
        //             .status(503)
        //             .body("unknow location to deal")
        //             .unwrap()
        //             .into_type());
        //     }
        // }
        // return Ok(Response::builder()
        //     .status(503)
        //     .body("unknow location")
        //     .unwrap()
        //     .into_type());
    }

    async fn operate(req: Request<RecvStream>) -> ProtResult<Response<RecvStream>> {
        // body的内容可能重新解密又再重新再加过密, 后续可考虑直接做数据
        let mut value = Self::inner_operate(req).await?;
        value.headers_mut().insert("server", "wmproxy");
        Ok(value)
    }

    pub async fn process<T>(
        http: Arc<Mutex<HttpConfig>>,
        inbound: T,
        addr: SocketAddr,
    ) -> ProxyResult<()>
    where
        T: AsyncRead + AsyncWrite + Unpin + std::marker::Send + 'static,
    {
        let oper = InnerHttpOper::new(http);
        tokio::spawn(async move {
            let mut server = Server::new_data(inbound, Some(addr), Arc::new(Mutex::new(oper)));
            if let Err(e) = server.incoming(Self::operate).await {
                log::info!("反向代理：处理信息时发生错误：{:?}", e);
            }
        });
        Ok(())
    }
}
