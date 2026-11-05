use cardbe_enterprise_server::{
    http::{router, AppState},
    store::Store,
};
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
};

struct Options {
    command: String,
    data: PathBuf,
    port: u16,
    origins: Vec<String>,
    organization: String,
    owner: String,
    device: String,
    organization_id: Option<String>,
    owner_id: Option<String>,
}

fn options() -> Result<Options, &'static str> {
    let mut args = std::env::args().skip(1);
    let command = args
        .next()
        .ok_or("Use bootstrap, serve or recover-owner; see enterprise-server/README.md")?;
    if !matches!(command.as_str(), "bootstrap" | "serve" | "recover-owner") {
        return Err("Expected bootstrap, serve or recover-owner");
    }
    let mut result = Options {
        command,
        data: PathBuf::from("enterprise-server/data/enterprise.sqlite3"),
        port: 9040,
        origins: vec![
            "http://127.0.0.1:1441".into(),
            "http://localhost:1441".into(),
        ],
        organization: "Demo organization".into(),
        owner: "Owner".into(),
        device: "Development device".into(),
        organization_id: None,
        owner_id: None,
    };
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("Missing option value")?;
        match arg.as_str() {
            "--data" => result.data = value.into(),
            "--port" => {
                result.port = value
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p >= 1024)
                    .ok_or("Port must be 1024..65535")?
            }
            "--origin" => {
                let authority = value
                    .strip_prefix("http://")
                    .ok_or("Prototype origins must use loopback HTTP")?;
                let (host, port) = authority.rsplit_once(':').ok_or("Origin requires a port")?;
                if !matches!(host, "127.0.0.1" | "localhost")
                    || port.parse::<u16>().ok().filter(|p| *p > 0).is_none()
                {
                    return Err(
                        "Origin must be http://127.0.0.1:<port> or http://localhost:<port>",
                    );
                }
                result.origins.push(value);
            }
            "--organization" => result.organization = value,
            "--owner" => result.owner = value,
            "--device" => result.device = value,
            "--organization-id" => result.organization_id = Some(value),
            "--owner-id" => result.owner_id = Some(value),
            _ => return Err("Unknown option"),
        }
    }
    if result.command == "recover-owner"
        && (result.organization_id.is_none() || result.owner_id.is_none())
    {
        return Err("recover-owner requires --organization-id and --owner-id");
    }
    Ok(result)
}

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let opts = options().map_err(str::to_owned)?;
    if opts.command == "recover-owner" && !opts.data.is_file() {
        return Err("Owner recovery requires an existing enterprise database".to_owned());
    }
    if let Some(parent) = opts.data.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|_| "Cannot create data directory")?;
    }
    // New databases are private to this OS account. Existing files retain their permissions.
    if !opts.data.exists() {
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        open.open(&opts.data)
            .map_err(|_| "Cannot create database")?;
    }
    let mut store = Store::open(&opts.data).map_err(|_| "Cannot initialize database")?;
    if opts.command != "serve" {
        let result = if opts.command == "bootstrap" {
            store.bootstrap(&opts.organization, &opts.owner, &opts.device)
        } else {
            store.recover_owner(
                opts.organization_id
                    .as_deref()
                    .ok_or("Missing organization ID")?,
                opts.owner_id.as_deref().ok_or("Missing owner ID")?,
                &opts.device,
            )
        }
        .map_err(|e| format!("Credential initialization failed: {e:?}"))?;
        eprintln!(
            "Development credentials shown once. Keep stdout private; tokens expire after 14 days."
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&result)
                .map_err(|_| "Cannot encode bootstrap response")?
        );
        return Ok(());
    }
    let listener = tokio::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, opts.port))
        .await
        .map_err(|_| "Cannot bind loopback port; an existing server may be running")?;
    eprintln!(
        "Cardbe enterprise prototype: http://127.0.0.1:{} (SQLite, local development only)",
        opts.port
    );
    let app = router(AppState::new(store, opts.port, opts.origins));
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|_| "Server stopped unexpectedly".to_owned())
}
