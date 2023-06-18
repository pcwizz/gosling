// standard
use std::default::Default;
use std::io::ErrorKind;
#[cfg(test)]
use std::io::{Read,Write};
use std::net::{SocketAddr, TcpListener};
use std::ops::Drop;
use std::option::Option;
use std::path::Path;
use std::string::ToString;
use std::sync::{atomic, Arc};
use std::time::Duration;

// extern crates
#[cfg(test)]
use serial_test::serial;
use socks::Socks5Stream;
use url::Host;

// internal crates
use crate::tor_control_stream::*;
use crate::tor_controller::*;
use crate::tor_crypto::*;
use crate::tor_process::*;
use crate::tor_provider::*;
use crate::tor_version::*;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("failed to create TorProcess object")]
    TorProcessCreationFailed(#[source] crate::tor_process::Error),

    #[error("failed to create ControlStream object")]
    ControlStreamCreationFailed(#[source] crate::tor_control_stream::Error),

    #[error("failed to create TorController object")]
    TorControllerCreationFailed(#[source] crate::tor_controller::Error),

    #[error("failed to authenticate with the tor process")]
    TorProcessAuthenticationFailed(#[source] crate::tor_controller::Error),

    #[error("failed to determine the tor process version")]
    GetInfoVersionFailed(#[source] crate::tor_controller::Error),

    #[error("tor process version to old; found {0} but must be at least {1}")]
    TorProcessTooOld(String, String),

    #[error("failed to register for STATUS_CLIENT and HS_DESC events")]
    SetEventsFailed(#[source] crate::tor_controller::Error),

    #[error("failed to delete unused onion service")]
    DelOnionFailed(#[source] crate::tor_controller::Error),

    #[error("failed waiting for async events")]
    WaitAsyncEventsFailed(#[source] crate::tor_controller::Error),

    #[error("failed to begin bootstrap")]
    SetConfDisableNetwork0Failed(#[source] crate::tor_controller::Error),

    #[error("failed to add client auth for onion service")]
    OnionClientAuthAddFailed(#[source] crate::tor_controller::Error),

    #[error("failed to remove client auth from onion service")]
    OnionClientAuthRemoveFailed(#[source] crate::tor_controller::Error),

    #[error("failed to get socks listener")]
    GetInfoNetListenersSocksFailed(#[source] crate::tor_controller::Error),

    #[error("no socks listeners available to connect through")]
    NoSocksListenersFound(),

    #[error("unable to connect to socks listener")]
    Socks5ConnectionFailed(#[source] std::io::Error),

    #[error("unable to bind TCP listener")]
    TcpListenerBindFailed(#[source] std::io::Error),

    #[error("unable to get TCP listener's local address")]
    TcpListenerLocalAddrFailed(#[source] std::io::Error),

    #[error("failed to create onion service")]
    AddOnionFailed(#[source] crate::tor_controller::Error),
}

//
// CircuitToken Implementation
//
pub struct TorDaemonCircuitToken {
    username: String,
    password: String,
}

impl TorDaemonCircuitToken {
    #[allow(dead_code)]
    pub fn new(first_party: Host) -> TorDaemonCircuitToken {
        const CIRCUIT_TOKEN_PASSWORD_LENGTH: usize = 32usize;
        let username = first_party.to_string();
        let password = generate_password(CIRCUIT_TOKEN_PASSWORD_LENGTH);

        TorDaemonCircuitToken { username, password }
    }
}

impl CircuitToken for TorDaemonCircuitToken {}

//
// TorDaemonOnionListener
//

pub struct TorDaemonOnionListener {
    listener: TcpListener,
    is_active: Arc<atomic::AtomicBool>,
    onion_addr: OnionAddr,
}

impl OnionListener for TorDaemonOnionListener {
    fn set_nonblocking(&self, nonblocking: bool) -> Result<(), std::io::Error> {
        self.listener.set_nonblocking(nonblocking)
    }

    fn accept(&self) -> Result<Option<OnionStream>, std::io::Error> {
        match self.listener.accept() {
            Ok((stream, _socket_addr)) => Ok(Some(OnionStream{
                stream,
                local_addr: Some(self.onion_addr.clone()),
                peer_addr: None,
            })),
            Err(err) => {
                if err.kind() == ErrorKind::WouldBlock {
                    Ok(None)
                } else {
                    Err(err)
                }
            }
        }
    }
}

impl Drop for TorDaemonOnionListener {
    fn drop(&mut self) {
        self.is_active.store(false, atomic::Ordering::Relaxed);
    }
}

pub struct LegacyTorClient {
    daemon: TorProcess,
    version: TorVersion,
    controller: TorController,
    socks_listener: Option<SocketAddr>,
    // list of open onion services and their is_active flag
    onion_services: Vec<(V3OnionServiceId, Arc<atomic::AtomicBool>)>,
}

impl LegacyTorClient {
    pub fn new(tor_bin_path: &Path, data_directory: &Path) -> Result<LegacyTorClient, Error> {
        // launch tor
        let daemon = TorProcess::new(tor_bin_path, data_directory)
            .map_err(Error::TorProcessCreationFailed)?;
        // open a control stream
        let control_stream =
            ControlStream::new(daemon.get_control_addr(), Duration::from_millis(16))
                .map_err(Error::ControlStreamCreationFailed)?;

        // create a controler
        let mut controller =
            TorController::new(control_stream).map_err(Error::TorControllerCreationFailed)?;

        // authenticate
        controller
            .authenticate(daemon.get_password())
            .map_err(Error::TorProcessAuthenticationFailed)?;

        // min required version for v3 client auth (see control-spec.txt)
        let min_required_version: TorVersion = TorVersion {
            major: 0u32,
            minor: 4u32,
            micro: 6u32,
            patch_level: 1u32,
            status_tag: None,
        };

        let version = controller
            .getinfo_version()
            .map_err(Error::GetInfoVersionFailed)?;

        if version < min_required_version {
            return Err(Error::TorProcessTooOld(
                version.to_string(),
                min_required_version.to_string(),
            ));
        }

        // register for STATUS_CLIENT async events
        controller
            .setevents(&["STATUS_CLIENT", "HS_DESC"])
            .map_err(Error::SetEventsFailed)?;

        Ok(LegacyTorClient {
            daemon,
            version,
            controller,
            socks_listener: None,
            onion_services: Default::default(),
        })
    }

    #[allow(dead_code)]
    pub fn version(&mut self) -> TorVersion {
        self.version.clone()
    }
}

impl TorProvider<TorDaemonCircuitToken, TorDaemonOnionListener> for LegacyTorClient {
    type Error = Error;

    fn update(&mut self) -> Result<Vec<TorEvent>, Error> {
        let mut i = 0;
        while i < self.onion_services.len() {
            // remove onion services with no active listeners
            if !self.onion_services[i].1.load(atomic::Ordering::Relaxed) {
                let entry = self.onion_services.swap_remove(i);
                let service_id = entry.0;

                self.controller
                    .del_onion(&service_id)
                    .map_err(Error::DelOnionFailed)?;
            } else {
                i += 1;
            }
        }

        let mut events: Vec<TorEvent> = Default::default();
        for async_event in self
            .controller
            .wait_async_events()
            .map_err(Error::WaitAsyncEventsFailed)?
            .iter()
        {
            match async_event {
                AsyncEvent::StatusClient {
                    severity,
                    action,
                    arguments,
                } => {
                    if severity == "NOTICE" && action == "BOOTSTRAP" {
                        let mut progress: u32 = 0;
                        let mut tag: String = Default::default();
                        let mut summary: String = Default::default();
                        for (key, val) in arguments.iter() {
                            match key.as_str() {
                                "PROGRESS" => progress = val.parse().unwrap_or(0u32),
                                "TAG" => tag = val.to_string(),
                                "SUMMARY" => summary = val.to_string(),
                                _ => {} // ignore unexpected arguments
                            }
                        }
                        events.push(TorEvent::BootstrapStatus {
                            progress,
                            tag,
                            summary,
                        });
                        if progress == 100u32 {
                            events.push(TorEvent::BootstrapComplete);
                        }
                    }
                }
                AsyncEvent::HsDesc { action, hs_address } => {
                    if action == "UPLOADED" {
                        events.push(TorEvent::OnionServicePublished {
                            service_id: hs_address.clone(),
                        });
                    }
                }
                AsyncEvent::Unknown { lines } => {
                    println!("Received Unknown Event:");
                    for line in lines.iter() {
                        println!(" {}", line);
                    }
                }
            }
        }

        for log_line in self.daemon.wait_log_lines().iter_mut() {
            events.push(TorEvent::LogReceived {
                line: std::mem::take(log_line),
            });
        }

        Ok(events)
    }

    fn bootstrap(&mut self) -> Result<(), Error> {
        self.controller
            .setconf(&[("DisableNetwork", "0")])
            .map_err(Error::SetConfDisableNetwork0Failed)
    }

    fn add_client_auth(
        &mut self,
        service_id: &V3OnionServiceId,
        client_auth: &X25519PrivateKey,
    ) -> Result<(), Error> {
        self.controller
            .onion_client_auth_add(service_id, client_auth, None, &Default::default())
            .map_err(Error::OnionClientAuthAddFailed)
    }

    fn remove_client_auth(&mut self, service_id: &V3OnionServiceId) -> Result<(), Error> {
        self.controller
            .onion_client_auth_remove(service_id)
            .map_err(Error::OnionClientAuthRemoveFailed)
    }

    // connect to an onion service and returns OnionStream
    fn connect(
        &mut self,
        service_id: &V3OnionServiceId,
        virt_port: u16,
        circuit: Option<TorDaemonCircuitToken>,
    ) -> Result<OnionStream, Error> {
        if self.socks_listener.is_none() {
            let mut listeners = self
                .controller
                .getinfo_net_listeners_socks()
                .map_err(Error::GetInfoNetListenersSocksFailed)?;
            if listeners.is_empty() {
                return Err(Error::NoSocksListenersFound());
            }
            self.socks_listener = Some(listeners.swap_remove(0));
        }

        let socks_listener = match self.socks_listener {
            Some(socks_listener) => socks_listener,
            None => unreachable!(),
        };

        // our onion domain
        let target =
            socks::TargetAddr::Domain(format!("{}.onion", service_id), virt_port);
        // readwrite stream
        let stream = match &circuit {
            None => Socks5Stream::connect(socks_listener, target),
            Some(circuit) => Socks5Stream::connect_with_password(
                socks_listener,
                target,
                &circuit.username,
                &circuit.password,
            ),
        }
        .map_err(Error::Socks5ConnectionFailed)?;

        Ok(OnionStream{
            stream: stream.into_inner(),
            local_addr: None,
            peer_addr: Some(TargetAddr::OnionService(OnionAddr::V3(service_id.clone(), virt_port))),
        })
    }

    // stand up an onion service and return an TorDaemonOnionListener
    fn listener(
        &mut self,
        private_key: &Ed25519PrivateKey,
        virt_port: u16,
        authorized_clients: Option<&[X25519PublicKey]>,
    ) -> Result<TorDaemonOnionListener, Error> {
        // try to bind to a local address, let OS pick our port
        let socket_addr = SocketAddr::from(([127, 0, 0, 1], 0u16));
        let listener = TcpListener::bind(socket_addr).map_err(Error::TcpListenerBindFailed)?;
        let socket_addr = listener
            .local_addr()
            .map_err(Error::TcpListenerLocalAddrFailed)?;

        let mut flags = AddOnionFlags {
            discard_pk: true,
            ..Default::default()
        };
        if authorized_clients.is_some() {
            flags.v3_auth = true;
        }

        let onion_addr = OnionAddr::V3(V3OnionServiceId::from_private_key(private_key), virt_port);

        // start onion service
        let (_, service_id) = self
            .controller
            .add_onion(
                Some(private_key),
                &flags,
                None,
                virt_port,
                Some(socket_addr),
                authorized_clients,
            )
            .map_err(Error::AddOnionFailed)?;

        let is_active = Arc::new(atomic::AtomicBool::new(true));
        self.onion_services
            .push((service_id, Arc::clone(&is_active)));

        Ok(TorDaemonOnionListener {
            listener,
            is_active,
            onion_addr,
        })
    }
}

#[test]
#[serial]
fn test_tor_manager() -> anyhow::Result<()> {
    let tor_path = which::which(tor_exe_name())?;
    let mut data_path = std::env::temp_dir();
    data_path.push("test_tor_manager");

    let mut tor = LegacyTorClient::new(&tor_path, &data_path)?;
    println!("version : {}", tor.version().to_string());
    tor.bootstrap()?;

    let mut received_log = false;
    let mut bootstrap_complete = false;
    while !bootstrap_complete {
        for event in tor.update()?.iter() {
            match event {
                TorEvent::BootstrapStatus {
                    progress,
                    tag,
                    summary,
                } => println!(
                    "BootstrapStatus: {{ progress: {}, tag: {}, summary: '{}' }}",
                    progress, tag, summary
                ),
                TorEvent::BootstrapComplete => {
                    println!("Bootstrap Complete!");
                    bootstrap_complete = true;
                }
                TorEvent::LogReceived { line } => {
                    received_log = true;
                    println!("--- {}", line);
                }
                _ => {}
            }
        }
    }
    assert!(
        received_log,
        "should have received a log line from tor daemon"
    );

    Ok(())
}

#[test]
#[serial]
fn test_onion_service() -> anyhow::Result<()> {
    let tor_path = which::which(tor_exe_name())?;
    let mut data_path = std::env::temp_dir();
    data_path.push("test_onion_service");

    let mut tor = LegacyTorClient::new(&tor_path, &data_path)?;

    // for 30secs for bootstrap
    tor.bootstrap()?;

    let mut bootstrap_complete = false;
    while !bootstrap_complete {
        for event in tor.update()?.iter() {
            match event {
                TorEvent::BootstrapStatus {
                    progress,
                    tag,
                    summary,
                } => println!(
                    "BootstrapStatus: {{ progress: {}, tag: {}, summary: '{}' }}",
                    progress, tag, summary
                ),
                TorEvent::BootstrapComplete => {
                    println!("Bootstrap Complete!");
                    bootstrap_complete = true;
                }
                TorEvent::LogReceived { line } => {
                    println!("--- {}", line);
                }
                _ => {}
            }
        }
    }

    // vanilla V3 onion service
    {
        // create an onion service for this test
        let private_key = Ed25519PrivateKey::generate();

        println!("Starting and listening to onion service");
        const VIRT_PORT: u16 = 42069u16;
        let listener = tor.listener(&private_key, VIRT_PORT, None)?;

        let mut onion_published = false;
        while !onion_published {
            for event in tor.update()?.iter() {
                match event {
                    TorEvent::LogReceived { line } => {
                        println!("--- {}", line);
                    }
                    TorEvent::OnionServicePublished { service_id } => {
                        let expected_service_id = V3OnionServiceId::from_private_key(&private_key);
                        if expected_service_id == *service_id {
                            println!("Onion Service {} published", service_id.to_string());
                            onion_published = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        const MESSAGE: &str = "Hello World!";

        {
            let service_id = V3OnionServiceId::from_private_key(&private_key);

            println!("Connecting to onion service");
            let mut client = tor.connect(&service_id, VIRT_PORT, None)?;
            println!("Client writing message: '{}'", MESSAGE);
            client.write_all(MESSAGE.as_bytes())?;
            client.flush()?;
            println!("End of client scope");
        }

        if let Some(mut server) = listener.accept()? {
            println!("Server reading message");
            let mut buffer = Vec::new();
            server.read_to_end(&mut buffer)?;
            let msg = String::from_utf8(buffer)?;

            assert!(MESSAGE == msg);
            println!("Message received: '{}'", msg);
        } else {
            panic!("no listener");
        }
    }

    // authenticated onion service
    {
        // create an onion service for this test
        let private_key = Ed25519PrivateKey::generate();

        let private_auth_key = X25519PrivateKey::generate();
        let public_auth_key = X25519PublicKey::from_private_key(&private_auth_key);

        println!("Starting and listening to authenticated onion service");
        const VIRT_PORT: u16 = 42069u16;
        let listener = tor.listener(&private_key, VIRT_PORT, Some(&[public_auth_key]))?;

        let mut onion_published = false;
        while !onion_published {
            for event in tor.update()?.iter() {
                match event {
                    TorEvent::LogReceived { line } => {
                        println!("--- {}", line);
                    }
                    TorEvent::OnionServicePublished { service_id } => {
                        let expected_service_id = V3OnionServiceId::from_private_key(&private_key);
                        if expected_service_id == *service_id {
                            println!(
                                "Authenticated Onion Service {} published",
                                service_id.to_string()
                            );
                            onion_published = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        const MESSAGE: &str = "Hello World!";

        {
            let service_id = V3OnionServiceId::from_private_key(&private_key);

            println!("Connecting to onion service (should fail)");
            assert!(
                tor.connect(&service_id, VIRT_PORT, None).is_err(),
                "should not able to connect to an authenticated onion service without auth key"
            );

            println!("Add auth key for onion service");
            tor.add_client_auth(&service_id, &private_auth_key)?;

            println!("Connecting to onion service with authentication");
            let mut client = tor.connect(&service_id, VIRT_PORT, None)?;

            println!("Client writing message: '{}'", MESSAGE);
            client.write_all(MESSAGE.as_bytes())?;
            client.flush()?;
            println!("End of client scope");

            println!("Remove auth key for onion service");
            tor.remove_client_auth(&service_id)?;
        }

        if let Some(mut server) = listener.accept()? {
            println!("Server reading message");
            let mut buffer = Vec::new();
            server.read_to_end(&mut buffer)?;
            let msg = String::from_utf8(buffer)?;

            assert!(MESSAGE == msg);
            println!("Message received: '{}'", msg);
        } else {
            panic!("no listener");
        }
    }
    Ok(())
}
