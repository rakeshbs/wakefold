//! An explicitly configured TLS listener for desktop clients.
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::DaemonError;
use axum::Router;
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use serde::Deserialize;
use tokio::{net::TcpListener, task::JoinSet};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    },
};
use wakefold::CancellationToken;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteConfig {
    certificate_chain: PathBuf,
    private_key: PathBuf,
    token_file: PathBuf,
}

pub(crate) struct Remote {
    tls: TlsAcceptor,
    pub(crate) bearer: Arc<str>,
}

impl RemoteConfig {
    pub(crate) fn load(self, directory: &Path) -> Result<Remote, DaemonError> {
        let certs = CertificateDer::pem_file_iter(directory.join(self.certificate_chain))?
            .collect::<Result<Vec<_>, _>>()?;
        let key = PrivateKeyDer::from_pem_file(directory.join(self.private_key))?;
        let config = ServerConfig::builder_with_provider(
            tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
        let token = std::fs::read_to_string(directory.join(self.token_file))?;
        let token = token.trim_end_matches(['\n', '\r']);
        if token.len() < 32 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("remote token file must contain a token of at least 32 printable non-space ASCII characters".into());
        }
        Ok(Remote {
            tls: TlsAcceptor::from(Arc::new(config)),
            bearer: Arc::from(token),
        })
    }
}

impl Remote {
    pub(crate) async fn serve(
        self,
        listener: TcpListener,
        app: Router,
        stop: CancellationToken,
    ) -> io::Result<()> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.cancelled()=>break,
                Some(_) = connections.join_next(), if !connections.is_empty()=>{},
                socket = listener.accept()=>{
                    let (socket,_)=socket?;
                    let tls=self.tls.clone();
                    let service=TowerToHyperService::new(app.clone());
                    connections.spawn(async move{
                        let Ok(Ok(stream))=tokio::time::timeout(Duration::from_secs(10),tls.accept(socket)).await else {return};
                        let _=http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(15))
                            .serve_connection(TokioIo::new(stream),service).with_upgrades().await;
                    });
                }
            }
        }
        connections.shutdown().await;
        Ok(())
    }
}
