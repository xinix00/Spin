//! De runner leest lokale env-bestanden; de control plane ontvangt uitsluitend status.
use crate::{executor, process::DockerExecutor};
use spin_core::{
    docker::{Docker, Executor, apps},
    validation::text,
};
use spin_domain::{self as d, TryClone, Wire, json::Value};
use std::{
    net::{SocketAddr, TcpStream, UdpSocket},
    path::PathBuf,
    time::{Duration, Instant},
};
type Result<T> = std::io::Result<T>;
fn io(e: impl std::fmt::Debug) -> std::io::Error {
    crate::client_net::error(e)
}
pub(crate) struct Config {
    directory: PathBuf,
    host: String,
}
impl Config {
    pub(crate) fn new(directory: String, mut host: String) -> Result<Self> {
        if host.trim().is_empty() {
            host = UdpSocket::bind("0.0.0.0:0")
                .and_then(|s| {
                    s.connect("192.0.2.1:9")?;
                    s.local_addr()
                })
                .map(|a| text(format_args!("{}", a.ip())).map_err(io))
                .unwrap_or_else(|_| d::try_string("127.0.0.1").map_err(io))?;
        }
        Ok(Self {
            directory: directory.into(),
            host,
        })
    }
    pub(crate) async fn invoke(
        &self,
        docker: &Docker,
        method: &str,
        payload: &d::protocol::AppPayload,
    ) -> Result<Value> {
        apps::network(&payload.session_id).map_err(io)?;
        match method {
            d::protocol::METHOD_START_APP => {
                let services =
                    spin_core::git::app_services(payload.services.try_clone().map_err(io)?)
                        .map_err(io)?;
                if services.is_empty() || services.len() > 32 {
                    return Err(io("app recipe requires 1 to 32 services"));
                }
                let network = apps::network(&payload.session_id).map_err(io)?;
                if control(docker, &["network", "inspect", &network])
                    .await
                    .is_err()
                {
                    control(
                        docker,
                        &[
                            "network",
                            "create",
                            "--label",
                            "spin.managed=true",
                            "--label",
                            &text(format_args!("spin.session_id={}", payload.session_id))
                                .map_err(io)?,
                            &network,
                        ],
                    )
                    .await?;
                }
                let mut running = d::List::new();
                for service in services.iter() {
                    let mut result = d::AppServiceRuntime {
                        service: service.name.try_clone().map_err(io)?,
                        host: self.host.try_clone().map_err(io)?,
                        status: d::try_string("starting").map_err(io)?,
                        ..Default::default()
                    };
                    match self.start(docker, payload, service).await {
                        Ok((id, ports)) => {
                            result.container_id = id;
                            result.ports = ports;
                            result.status = d::try_string("running").map_err(io)?;
                            result.started_at = Some(crate::server::timestamp()?);
                        }
                        Err(error) => {
                            result.status = d::try_string("error").map_err(io)?;
                            result.error = text(format_args!("{error}")).map_err(io)?;
                        }
                    }
                    if !service.image.is_empty()
                        && !result.ports.is_empty()
                        && result.error.is_empty()
                    {
                        let deadline = Instant::now() + Duration::from_secs(15);
                        while Instant::now() < deadline {
                            if reachable(&result) {
                                result.reachable = true;
                                break;
                            }
                            let next = Instant::now() + Duration::from_millis(500);
                            while Instant::now() < next {
                                executor::next_round().await;
                            }
                        }
                    }
                    running.push(result).map_err(io)?;
                }
                d::protocol::AppStatusResult { services: running }
                    .to_value()
                    .map_err(io)
            }
            d::protocol::METHOD_STOP_APP => {
                self.stop(docker, &payload.session_id).await?;
                Ok(Value::Null)
            }
            d::protocol::METHOD_APP_STATUS => {
                let mut services = self.status(docker, &payload.session_id).await?;
                for service in services.as_mut_slice() {
                    if service.status == "running" {
                        service.reachable = reachable(service);
                    }
                }
                d::protocol::AppStatusResult { services }
                    .to_value()
                    .map_err(io)
            }
            d::protocol::METHOD_APP_LOGS => {
                let name = apps::container(&payload.session_id, &payload.service).map_err(io)?;
                let output = control(
                    docker,
                    &[
                        "logs",
                        "--tail",
                        &text(format_args!(
                            "{}",
                            if payload.tail <= 0 {
                                200
                            } else {
                                payload.tail.min(10000)
                            }
                        ))
                        .map_err(io)?,
                        "--timestamps",
                        &name,
                    ],
                )
                .await?;
                d::protocol::AppLogsResult { output }.to_value().map_err(io)
            }
            _ => Err(io("unknown app operation")),
        }
    }
    async fn start(
        &self,
        docker: &Docker,
        payload: &d::protocol::AppPayload,
        service: &d::AppService,
    ) -> Result<(String, d::WireMap<i64>)> {
        let name = apps::container(&payload.session_id, &service.name).map_err(io)?;
        let _ = control(docker, &["rm", "-f", &name]).await;
        let file = if service.env.is_empty() {
            None
        } else {
            let file = self
                .directory
                .join(text(format_args!("{}.env", service.env)).map_err(io)?);
            if !file.is_file() {
                return Err(io("app env file is missing on this runner"));
            }
            Some(file)
        };
        let path = file
            .as_ref()
            .map(|p| p.to_str().ok_or_else(|| io("non-UTF8 app env path")))
            .transpose()?;
        let lease = docker.track_cleanup(&name).map_err(io)?;
        let command = docker
            .start_app_service(
                &payload.runtime,
                &payload.session_id,
                service,
                &payload.hosts,
                path,
            )
            .map_err(io)?;
        let output = DockerExecutor.run(command).await.map_err(io)?;
        if output.code != 0 {
            return Err(io((
                "app start failed",
                spin_core::docker::utf8(&output.bytes, true).map_err(io)?,
            )));
        }
        let id = control(docker, &["inspect", "--format", "{{.Id}}", &name]).await?;
        let ports = ports(docker, &id).await?;
        // This is now a durable service, discovered by labels after runner restart.
        lease.complete();
        Ok((id, ports))
    }
    async fn status(
        &self,
        docker: &Docker,
        session: &str,
    ) -> Result<d::List<d::AppServiceRuntime>> {
        let output = control(
            docker,
            &[
                "ps",
                "-a",
                "--no-trunc",
                "--filter",
                "label=spin.kind=app",
                "--filter",
                &text(format_args!("label=spin.session_id={session}")).map_err(io)?,
                "--format",
                "{{.ID}}\t{{.Label \"spin.service\"}}\t{{.State}}\t{{.Status}}",
            ],
        )
        .await?;
        let mut result = d::List::new();
        for line in output.lines() {
            let mut fields = line.split('\t');
            let Some(id) = fields.next().filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some(name) = fields.next() else {
                continue;
            };
            let Some(status) = fields.next() else {
                continue;
            };
            let detail = fields.next().unwrap_or("");
            if result.len() >= 256 {
                return Err(io("too many app service containers"));
            }
            result
                .push(d::AppServiceRuntime {
                    service: d::try_string(name).map_err(io)?,
                    container_id: d::try_string(id).map_err(io)?,
                    status: d::try_string(status).map_err(io)?,
                    host: self.host.try_clone().map_err(io)?,
                    ports: if status == "running" {
                        ports(docker, id).await?
                    } else {
                        Default::default()
                    },
                    error: if status == "running" {
                        String::new()
                    } else {
                        d::try_string(detail).map_err(io)?
                    },
                    ..Default::default()
                })
                .map_err(io)?;
        }
        result
            .as_mut_slice()
            .sort_by(|a, b| a.service.cmp(&b.service));
        Ok(result)
    }
    pub(crate) async fn stop(&self, docker: &Docker, session: &str) -> Result<()> {
        for service in self.status(docker, session).await?.iter() {
            control(docker, &["rm", "-f", &service.container_id]).await?;
        }
        let network = apps::network(session).map_err(io)?;
        if control(docker, &["network", "inspect", &network])
            .await
            .is_ok()
        {
            control(docker, &["network", "rm", &network]).await?;
        }
        Ok(())
    }
}
async fn control(docker: &Docker, args: &[&str]) -> Result<String> {
    docker.control(&mut DockerExecutor, args).await.map_err(io)
}
async fn ports(docker: &Docker, id: &str) -> Result<d::WireMap<i64>> {
    let json = control(
        docker,
        &["inspect", "--format", "{{json .NetworkSettings.Ports}}", id],
    )
    .await?;
    let value = Value::from_json(json.as_bytes()).map_err(io)?;
    let mut ports = d::WireMap::new();
    if let Some(object) = value.as_object() {
        for (name, bindings) in object.iter() {
            if let Some(bindings) = bindings.as_array() {
                for binding in bindings {
                    if let Some(port) = binding
                        .as_object()
                        .and_then(|o| o.get("HostPort"))
                        .and_then(Value::as_str)
                        .and_then(|p| p.parse::<i64>().ok())
                        .filter(|p| (1..=65535).contains(p))
                    {
                        ports
                            .insert(
                                d::try_string(name.strip_suffix("/tcp").unwrap_or(name))
                                    .map_err(io)?,
                                port,
                            )
                            .map_err(io)?;
                        break;
                    }
                }
            }
        }
    }
    Ok(ports)
}
fn reachable(service: &d::AppServiceRuntime) -> bool {
    service.ports.iter().take(16).any(|(_, port)| {
        u16::try_from(*port).is_ok_and(|port| {
            TcpStream::connect_timeout(
                &SocketAddr::from(([127, 0, 0, 1], port)),
                Duration::from_millis(50),
            )
            .is_ok()
        })
    })
}
