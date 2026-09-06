//! `openadr vtn` — running a VTN from the command line.

use std::sync::Arc;

use openadr::model::ClientId;
use openadr::schema::Policy;
use openadr::vtn::{
    Vtn,
    auth::{AnonymousAuth, StaticTokenAuth},
    store::{MemoryStorage, SharedStorage},
};

pub fn print_help() {
    println!(
        "openadr vtn — run a Virtual Top Node

USAGE:
    openadr vtn [OPTIONS]

OPTIONS:
    --listen <ADDR>            address to bind            [default: 0.0.0.0:3000]
    --public-url <URL>         how clients reach this VTN, for /auth/server
    --base-path <PATH>         mount point                [default: {}]
    --database <TARGET>        SQLite file path, or a postgres:// URL
    --payload-validation <P>   off | warn | strict        [default: warn]
    --no-cache                 disable ETag/If-None-Match
    --ephemeral                acknowledge in-memory storage on a public address
    --webhooks                 deliver notifications to subscriber callbacks
    --webhook-key <SECRET>     sign webhook payloads with this HMAC key
    --webhook-allow-private    accept callback URLs on loopback and private addresses,
                               and plain http. For local development only — it re-enables
                               exactly the request forgery the default refuses
    --breaker-threshold <N>    cut a subscriber off after N notifications abandoned
                               in a row; 0 disables the breaker        [default: 3]
    --breaker-cooldown <SECS>  how long a cut-off subscriber stays cut off before
                               one probe is let through                [default: 900]
    --mqtt-broker <URL>        publish notifications to this broker      [mqtt]
                               mqtt://host:1883 or mqtts://host:8883
    --mqtt-advertise <URL>     broker URL clients are told to use, if it differs
                               from --mqtt-broker (the VTN may reach it privately)
    --mqtt-username <NAME>     broker credentials for the VTN's own connection
    --mqtt-password <SECRET>
    --mqtt-topic-prefix <P>    prefix for topic names   [default: openadr3/<version>]
    --mqtt-retain              publish with the retain flag
    --mqtt-bl-client <ID>      a clientID allowed to subscribe to collection-wide
                               topics via /internal/mqtt/acl. Repeatable.
    --mqtt-publisher-client <ID>
                               the clientID this VTN's own publisher connects as.
                               Needed only when the broker authenticates the VTN
                               through /internal/mqtt/auth like every other client
                               — without it the broker refuses its own VTN.
    --report-retention <FOR>   delete reports older than this. Off by default: a report
                               is settlement data. Accepts 90d, 12h, 30m, 3600s or a
                               bare number of seconds
    --tls-cert <PEM>           serve HTTPS with this certificate chain          [tls]
    --tls-key <PEM>            and this private key (PKCS#8, PKCS#1 or SEC1)
    --tls-client-ca <PEM>      refuse any connection whose client certificate
                               this CA did not issue
    --tls-client-optional      ask for a client certificate but serve a peer
                               that presents none. For a migration, and nothing else
    --mdns                     advertise this VTN on the local network as
                               _openadr3._tcp, so a VEN on the same site finds
                               it without being told a URL                [mdns]
    --mdns-name <NAME>         DNS-SD instance name          [default: openadr-vtn]
    --mdns-host <HOST>         .local hostname to announce   [default: <NAME>.local]
    --mdns-program <NAME>      a programName local VENs should follow. Repeatable.

RETENTION:
    `report` is the only object in OpenADR that grows without bound. Every other one is created
    by business logic and deleted by it; a report is created by a fleet, on a schedule the VTN
    itself asked for, and nothing in the protocol ever removes one. A thousand resources filing
    quarter-hourly compliance data is about thirty-five million rows a year.

    --report-retention deletes what is older. It is off by default and says so at start-up when
    it is on, because reports are settlement data and forgetting them quietly is a billing
    dispute. Deletion is permanent, announces nothing — there is no OpenADR notification saying
    that a report you filed has been forgotten — and sweeps oldest-first in bounded batches, so
    the first pass over a year of data is many small transactions rather than one long lock.

    GET /health and GET /metrics both report how many reports are stored and how old the oldest
    is, which is how a working sweeper is told apart from one that is configured and not
    running.

TRANSPORT SECURITY:
    --tls-cert and --tls-key serve the API over TLS 1.2+ directly, so the single-binary
    deployment does not need a reverse proxy to be reachable over anything but plaintext.
    ALPN offers h2 and http/1.1.

    --tls-client-ca is the other direction and it is a *network* gate rather than an identity:
    a peer whose certificate that CA did not issue is refused during the handshake, before a
    byte of HTTP is parsed. Who the caller *is* still comes from the credential — see
    AUTHENTICATION below — because a VTN with two identity sources is a VTN with two answers.
    Fluvius' NetFlex profile is the deployment that wants the pair: mutual TLS at the edge and
    --bl-token/--ven-token behind it, since the profile forbids OAuth2.

    With TLS on, /auth/server and the mDNS record advertise https:// rather than http://.

LOCAL DISCOVERY:
    A VEN inside a customer site SHOULD be able to find the VTN on the same network without being
    told a URL, and a VTN for that site SHOULD advertise itself. --mdns does that: the service type
    is `_openadr3._tcp`, and the TXT record carries version, base_path, local_url, program_names
    and requires_auth exactly as the specification spells them. `openadr discover` is the other end.

    The advertised URL uses the .local hostname rather than an address, because a DHCP lease
    changes and a name does not. Pass --public-url to advertise something else entirely.

WEBHOOKS:
    A subscriber that stops answering costs `max_attempts` HTTP round trips on every write, for
    ever, because giving up is per-notification and cannot see the pattern. The circuit breaker
    stops that: after --breaker-threshold notifications abandoned in a row, new ones for that
    subscription are not queued at all until --breaker-cooldown has passed and one probe succeeds.

    A cut-off subscriber queues nothing, so it is deliberately visible rather than silent:
    GET /admin/subscribers lists it with the error its endpoint returned, GET /health counts it,
    and POST /admin/outbox/retry closes every breaker and revives the backlog in one call.

MQTT:
    3.1 added push over a broker so a VEN behind a residential firewall can be reached at all.
    --mqtt-broker turns it on: GET /notifiers starts naming the broker, the topic endpoints start
    answering, and every write publishes a private copy to each entitled VEN's topic.

    Point the broker's authorization callbacks at POST /internal/mqtt/auth and
    POST /internal/mqtt/acl — without them any VEN can subscribe to any other VEN's topic, which
    the specification requires the VTN to prevent. `deploy/emqx.conf` and `deploy/mosquitto.conf`
    are working configurations, and `deploy/compose.yaml` runs the pair.

    A broker configured that way authenticates the VTN too, so the VTN needs credentials the
    callback accepts — --mqtt-username and --mqtt-password — and its clientID named in
    --mqtt-publisher-client, which is the one identity /internal/mqtt/acl lets publish. Without it
    the broker refuses its own VTN and the fan-out reconnects for ever against `NotAuthorized`.

AUTHENTICATION (pick one):
    --anonymous                serve unauthenticated readers (public tariff mode)
    --bl-token <TOKEN>         accept this token as business logic
    --ven-token <TOKEN>        accept this token as a VEN
    --client <ID:SECRET:ROLE>  register a client for this VTN's own /auth/token;
                               ROLE is `bl` or `ven`. Repeatable.  [internal-auth]
    --client-hashed <ID:HASH:ROLE>
                               the same, with an Argon2 PHC hash instead of the
                               secret — see `openadr hash-secret`. Keeps the
                               plaintext out of the process table. Repeatable.
    --token-key <SECRET>       sign issued tokens with this key, so several instances
                               accept each other's                 [internal-auth]
    --token-ttl <SECONDS>      lifetime of an issued token         [internal-auth]
    --jwks-url <URL>           validate tokens against this key set [external-auth]
    --token-url <URL>          the authorization server's token endpoint
    --issuer <ISS>             require this `iss` claim            [external-auth]
    --audience <AUD>           require this `aud` claim. Repeatable [external-auth]

    --bl-token/--ven-token are pre-shared secrets: fine for development and for a gateway that
    has already authenticated the caller, not an OAuth2 grant. --client makes this VTN its own
    authorization server; --jwks-url points it at somebody else's.

    Prefer --client-hashed in anything but development: --client puts a plaintext secret in the
    process table, where every other process on the machine can read it.

STORAGE:
    A postgres:// URL selects PostgreSQL, anything else is a SQLite file path. Postgres is the
    choice for a multi-tenant or utility-scale VTN: several server instances can share it, and its
    notification queue both scales across dispatchers and wakes them the instant a write commits.
    SQLite is the choice for a site controller or a single-tenant pilot — one binary, one file.

    Without --database the VTN keeps everything in memory and loses it on restart. Binding a
    non-loopback address with in-memory storage requires --ephemeral, so that shipping a VTN that
    silently forgets its programmes has to be a deliberate act.

ENVIRONMENT:
    RUST_LOG                   tracing filter, e.g. openadr=debug
",
        openadr::DEFAULT_BASE_PATH,
    );
}

pub fn run_vtn(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut listen = "0.0.0.0:3000".to_string();
    let mut public_url: Option<String> = None;
    let mut base_path = openadr::DEFAULT_BASE_PATH.to_string();
    let mut anonymous = false;
    let mut bl_token: Option<String> = None;
    let mut ven_token: Option<String> = None;
    let mut clients: Vec<String> = Vec::new();
    let mut hashed_clients: Vec<String> = Vec::new();
    let mut token_key: Option<String> = None;
    let mut token_ttl: Option<u64> = None;
    let mut jwks_url: Option<String> = None;
    let mut token_url_override: Option<String> = None;
    let mut issuer: Option<String> = None;
    let mut audiences: Vec<String> = Vec::new();
    let mut policy = Policy::Warn;
    let mut caching = true;
    let mut database: Option<String> = None;
    let mut ephemeral = false;
    let mut webhooks = false;
    let mut webhook_key: Option<String> = None;
    let mut webhook_allow_private = false;
    let mut breaker_threshold: Option<u32> = None;
    let mut breaker_cooldown: Option<u64> = None;
    let mut mqtt_broker: Option<String> = None;
    let mut mqtt_advertise: Option<String> = None;
    let mut mqtt_username: Option<String> = None;
    let mut mqtt_password: Option<String> = None;
    let mut mqtt_topic_prefix: Option<String> = None;
    let mut mqtt_retain = false;
    let mut mqtt_bl_clients: Vec<String> = Vec::new();
    let mut mqtt_publisher_client: Option<String> = None;
    let mut report_retention: Option<String> = None;
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;
    let mut tls_client_ca: Option<String> = None;
    let mut tls_client_optional = false;
    let mut mdns = false;
    let mut mdns_name: Option<String> = None;
    let mut mdns_host: Option<String> = None;
    let mut mdns_programs: Vec<String> = Vec::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--listen" => listen = value()?,
            "--public-url" => public_url = Some(value()?),
            "--base-path" => base_path = value()?,
            "--anonymous" => anonymous = true,
            "--bl-token" => bl_token = Some(value()?),
            "--ven-token" => ven_token = Some(value()?),
            "--client" => clients.push(value()?),
            "--client-hashed" => hashed_clients.push(value()?),
            "--token-key" => token_key = Some(value()?),
            "--token-ttl" => token_ttl = Some(value()?.parse()?),
            "--jwks-url" => jwks_url = Some(value()?),
            "--token-url" => token_url_override = Some(value()?),
            "--issuer" => issuer = Some(value()?),
            "--audience" => audiences.push(value()?),
            "--payload-validation" => policy = value()?.parse()?,
            "--no-cache" => caching = false,
            "--database" => database = Some(value()?),
            "--ephemeral" => ephemeral = true,
            "--webhooks" => webhooks = true,
            "--webhook-key" => {
                webhook_key = Some(value()?);
                webhooks = true;
            }
            "--webhook-allow-private" => {
                webhook_allow_private = true;
                webhooks = true;
            }
            "--breaker-threshold" => breaker_threshold = Some(value()?.parse()?),
            "--breaker-cooldown" => breaker_cooldown = Some(value()?.parse()?),
            "--mqtt-broker" => mqtt_broker = Some(value()?),
            "--mqtt-advertise" => mqtt_advertise = Some(value()?),
            "--mqtt-username" => mqtt_username = Some(value()?),
            "--mqtt-password" => mqtt_password = Some(value()?),
            "--mqtt-topic-prefix" => mqtt_topic_prefix = Some(value()?),
            "--mqtt-retain" => mqtt_retain = true,
            "--mqtt-bl-client" => mqtt_bl_clients.push(value()?),
            "--mqtt-publisher-client" => mqtt_publisher_client = Some(value()?),
            "--report-retention" => report_retention = Some(value()?),
            "--tls-cert" => tls_cert = Some(value()?),
            "--tls-key" => tls_key = Some(value()?),
            "--tls-client-ca" => tls_client_ca = Some(value()?),
            "--tls-client-optional" => tls_client_optional = true,
            "--mdns" => mdns = true,
            "--mdns-name" => {
                mdns_name = Some(value()?);
                mdns = true;
            }
            "--mdns-host" => {
                mdns_host = Some(value()?);
                mdns = true;
            }
            "--mdns-program" => {
                mdns_programs.push(value()?);
                mdns = true;
            }
            other => return Err(format!("unknown option {other:?}").into()),
        }
    }

    // TLS is decided before anything is advertised: `/auth/server` and the mDNS record both name a
    // URL, and a client sent to `http://` on a listener that only speaks TLS fails at the transport
    // with nothing to read.
    let serve_tls = tls_cert.is_some() || tls_key.is_some() || tls_client_ca.is_some();
    if serve_tls && (tls_cert.is_none() || tls_key.is_none()) {
        return Err("--tls-cert and --tls-key are both needed to serve TLS".into());
    }
    if tls_client_optional && tls_client_ca.is_none() {
        return Err("--tls-client-optional without --tls-client-ca asks for nothing".into());
    }
    let scheme = if serve_tls { "https" } else { "http" };

    // What a *client* would have to type. `--listen 0.0.0.0:3000` is a bind address, not a URL:
    // advertising it from `/auth/server` sends every client to an address that means "everywhere"
    // on its own machine.
    let base = public_url
        .clone()
        .unwrap_or_else(|| format!("{scheme}://{}", advertised_host(&listen)));
    let token_url = token_url_override
        .clone()
        .unwrap_or_else(|| format!("{}{base_path}/auth/token", base.trim_end_matches('/')));

    // Only the mDNS record reads this — see `requires_auth` below.
    #[cfg(feature = "mdns")]
    let static_tokens_configured = bl_token.is_some()
        || ven_token.is_some()
        || !clients.is_empty()
        || !hashed_clients.is_empty();

    let authenticator = build_authenticator(AuthOptions {
        token_url,
        anonymous,
        bl_token,
        ven_token,
        clients,
        hashed_clients,
        token_key,
        token_ttl,
        jwks_url,
        issuer,
        audiences,
    })?;

    // What the mDNS record says about credentials. `--anonymous` alone is a read-only public VTN;
    // anything else expects a token, and a VEN told otherwise loops on `401`s with no explanation.
    //
    // Gated, because the record is the only reader: a build without `mdns` computing it is an
    // unused binding, and CI compiles every feature combination with `-D warnings`.
    #[cfg(feature = "mdns")]
    let requires_auth = !(anonymous && !static_tokens_configured);

    let storage = resolve_storage(database.as_deref(), &listen, ephemeral)?;

    // The same policy the API applies when a subscription is created and the transport applies
    // before every delivery, so a URL cannot be accepted at one and refused at the other.
    let callback_policy = if webhook_allow_private {
        eprintln!(
            "warning: --webhook-allow-private accepts callback URLs on loopback and private \n\
             addresses. That is the server-side request forgery the default refuses: any client \n\
             that can create a subscription can make this VTN fetch a URL inside your network."
        );
        openadr::vtn::notify::CallbackPolicy::permissive()
    } else {
        openadr::vtn::notify::CallbackPolicy::default()
    };

    let mut config = openadr::vtn::VtnConfig {
        base_path,
        payload_policy: policy,
        http_caching: caching,
        callback_policy: callback_policy.clone(),
        ..Default::default()
    };
    if let Some(spec) = &report_retention {
        config.retention.reports = Some(parse_duration(spec)?);
    }
    if let Some(threshold) = breaker_threshold {
        config.dispatch.breaker = if threshold == 0 {
            openadr::vtn::store::BreakerPolicy::disabled()
        } else {
            openadr::vtn::store::BreakerPolicy {
                threshold,
                ..config.dispatch.breaker
            }
        };
    }
    if let Some(seconds) = breaker_cooldown {
        config.dispatch.breaker.cooldown = std::time::Duration::from_secs(seconds);
    }
    config.mqtt_topic_prefix =
        mqtt_topic_prefix.unwrap_or_else(|| format!("openadr3/{}", openadr::SPEC_VERSION));
    for id in &mqtt_bl_clients {
        config.mqtt_business_logic_clients.push(
            id.parse()
                .map_err(|e| format!("--mqtt-bl-client {id:?}: {e}"))?,
        );
    }
    if let Some(id) = &mqtt_publisher_client {
        config.mqtt_publisher_client = Some(
            id.parse()
                .map_err(|e| format!("--mqtt-publisher-client {id:?}: {e}"))?,
        );
    }
    if let Some(broker) = &mqtt_broker {
        // What the VTN connects to and what a client is told to connect to may differ: the broker
        // is often reachable privately from the VTN and publicly from everybody else.
        config.mqtt = Some(openadr::model::MqttNotifierBinding {
            uris: vec![mqtt_advertise.clone().unwrap_or_else(|| broker.clone())],
            serialization: openadr::model::Serialization::Json,
            authentication: openadr::model::MqttAuthentication::Oauth2BearerToken {
                username: openadr::model::MqttAuthentication::CLIENT_ID_PLACEHOLDER.to_string(),
            },
        });
        if config.mqtt_business_logic_clients.is_empty() {
            tracing::warn!(
                "no --mqtt-bl-client: no client may subscribe to collection-wide topics through \
                 /internal/mqtt/acl. VENs still reach their own."
            );
        }
        if mqtt_username.is_some() && config.mqtt_publisher_client.is_none() {
            // The broker is being given credentials for this VTN, which means it authenticates the
            // VTN — and `/internal/mqtt/acl` refuses every publish it has not been told about.
            tracing::warn!(
                "--mqtt-username is set but --mqtt-publisher-client is not: if this broker \
                 authorizes through /internal/mqtt/acl it will refuse this VTN's own publishes, \
                 and the fan-out will reconnect for ever against `NotAuthorized`."
            );
        }
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            // `mut` only when a notifier can be attached, which is the `webhook` feature.
            #[cfg_attr(not(any(feature = "webhook", feature = "mqtt")), allow(unused_mut))]
            let mut builder = Vtn::builder()
                .storage(storage.resolve().await?)
                .authenticator(authenticator)
                .config(config);

            // One `Notifiers` holds both transports and routes each queued delivery by its channel,
            // so a broker entry is never handed to the webhook transport and vice versa.
            #[cfg_attr(not(any(feature = "webhook", feature = "mqtt")), allow(unused_mut))]
            let mut transports = openadr::vtn::notify::Notifiers::new();

            if webhooks {
                #[cfg(feature = "webhook")]
                {
                    use openadr::vtn::notify::{WebhookConfig, WebhookNotifier};
                    if webhook_key.is_none() {
                        tracing::warn!(
                            "webhook payloads are unsigned; pass --webhook-key so receivers can \
                             authenticate this VTN"
                        );
                    }
                    transports = transports.with(WebhookNotifier::shared(WebhookConfig {
                        signing_key: webhook_key,
                        policy: callback_policy,
                        ..Default::default()
                    })?);
                    tracing::info!("webhook delivery enabled");
                }
                #[cfg(not(feature = "webhook"))]
                {
                    drop(webhook_key);
                    return Err(
                        "--webhooks needs the `webhook` feature; rebuild with --features webhook"
                            .into(),
                    );
                }
            }

            if let Some(url) = mqtt_broker {
                #[cfg(feature = "mqtt")]
                {
                    use openadr::vtn::notify::{MqttConfig, MqttNotifier};
                    transports = transports.with(MqttNotifier::connect(MqttConfig {
                        url: url.clone(),
                        username: mqtt_username,
                        password: mqtt_password,
                        retain: mqtt_retain,
                        ..Default::default()
                    })?);
                    tracing::info!(broker = %url, "MQTT delivery enabled");
                }
                #[cfg(not(feature = "mqtt"))]
                {
                    drop((url, mqtt_username, mqtt_password, mqtt_retain));
                    return Err(
                        "--mqtt-broker needs the `mqtt` feature; rebuild with --features mqtt"
                            .into(),
                    );
                }
            }

            if !transports.is_empty() {
                builder = builder.notifier(transports.shared());
            }

            let vtn = builder.build();

            // Held for as long as the server runs: dropping it withdraws the record, and an mDNS
            // record outlives the process that published it otherwise.
            #[cfg(feature = "mdns")]
            let _advertisement = match mdns {
                true => Some(advertise(
                    &listen,
                    &vtn.state().config.base_path,
                    mdns_name,
                    mdns_host,
                    mdns_programs,
                    requires_auth,
                    serve_tls,
                )?),
                false => None,
            };
            #[cfg(not(feature = "mdns"))]
            if mdns {
                drop((mdns_name, mdns_host, mdns_programs));
                return Err("--mdns needs the `mdns` feature; rebuild with --features mdns".into());
            }

            if serve_tls {
                #[cfg(feature = "tls")]
                {
                    use openadr::vtn::tls::TlsConfig;
                    let mut tls = TlsConfig::from_pem_files(
                        tls_cert.as_deref().expect("checked above"),
                        tls_key.as_deref().expect("checked above"),
                    )?;
                    if let Some(ca) = &tls_client_ca {
                        tls = tls.with_client_ca_file(ca)?;
                        if tls_client_optional {
                            tls = tls.allowing_anonymous_clients();
                        }
                    }
                    vtn.serve_tls(listen.as_str(), tls).await?;
                }
                #[cfg(not(feature = "tls"))]
                return Err(
                    "--tls-cert needs the `tls` feature; rebuild with --features tls".into(),
                );
            } else {
                vtn.serve(listen.as_str()).await?;
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        })
}

/// Everything the authentication flags asked for.
///
/// Several fields are only read by a feature-gated branch, so a build without those features would
/// otherwise warn about them — which would be the warning telling the truth for the wrong reason.
#[allow(dead_code)]
struct AuthOptions {
    token_url: String,
    anonymous: bool,
    bl_token: Option<String>,
    ven_token: Option<String>,
    clients: Vec<String>,
    hashed_clients: Vec<String>,
    token_key: Option<String>,
    token_ttl: Option<u64>,
    jwks_url: Option<String>,
    issuer: Option<String>,
    audiences: Vec<String>,
}

/// Pick an authenticator, refusing combinations that would silently ignore half of them.
fn build_authenticator(
    options: AuthOptions,
) -> Result<Arc<dyn openadr::vtn::auth::Authenticator>, Box<dyn std::error::Error>> {
    let static_tokens = options.bl_token.is_some() || options.ven_token.is_some();
    let registered = !options.clients.is_empty() || !options.hashed_clients.is_empty();
    let chosen = usize::from(static_tokens)
        + usize::from(registered)
        + usize::from(options.jwks_url.is_some());
    if chosen > 1 {
        return Err("pick one of --bl-token/--ven-token, --client and --jwks-url".into());
    }

    if let Some(jwks_url) = options.jwks_url {
        #[cfg(feature = "external-auth")]
        {
            use openadr::vtn::auth::{JwtAuthenticator, JwtConfig};
            let mut config = JwtConfig::new(jwks_url, options.token_url);
            if let Some(issuer) = options.issuer {
                config = config.with_issuer(issuer);
            }
            if !options.audiences.is_empty() {
                config = config.with_audiences(options.audiences);
            } else {
                tracing::warn!(
                    "no --audience configured: any token this authorization server issued, for \
                     any application, will be accepted here"
                );
            }
            return Ok(Arc::new(JwtAuthenticator::new(config)?));
        }
        #[cfg(not(feature = "external-auth"))]
        {
            drop(jwks_url);
            return Err(
                "--jwks-url needs the `external-auth` feature; rebuild with --features external-auth"
                    .into(),
            );
        }
    }

    if registered {
        #[cfg(feature = "internal-auth")]
        {
            use openadr::vtn::auth::{InternalAuth, Scope, Scopes};
            let mut auth = InternalAuth::builder(options.token_url);
            for (spec, already_hashed) in options
                .clients
                .iter()
                .map(|s| (s, false))
                .chain(options.hashed_clients.iter().map(|s| (s, true)))
            {
                // `id:secret:role`. Split from the right twice so a secret may contain colons.
                let (rest, role) = spec
                    .rsplit_once(':')
                    .ok_or_else(|| format!("--client {spec:?} is not id:secret:role"))?;
                let (id, secret) = rest
                    .split_once(':')
                    .ok_or_else(|| format!("--client {spec:?} is not id:secret:role"))?;
                let scopes = match role {
                    "bl" => Scopes::new(Scope::BUSINESS_LOGIC),
                    "ven" => Scopes::new(Scope::VEN),
                    other => {
                        return Err(
                            format!("--client role must be bl or ven, not {other:?}").into()
                        );
                    }
                };
                auth = if already_hashed {
                    auth.client_hashed(id, secret, scopes)
                } else {
                    auth.client(id, secret, scopes)?
                };
            }
            if let Some(key) = options.token_key {
                auth = auth.signing_key(key.into_bytes());
            } else {
                tracing::warn!(
                    "no --token-key: this instance signs with a fresh random key, so a restart \
                     invalidates outstanding tokens and a second instance will not accept them"
                );
            }
            if let Some(ttl) = options.token_ttl {
                auth = auth.ttl(std::time::Duration::from_secs(ttl));
            }
            if options.anonymous {
                auth = auth.allowing_anonymous();
            }
            return Ok(Arc::new(auth.build()));
        }
        #[cfg(not(feature = "internal-auth"))]
        {
            return Err(
                "--client/--client-hashed need the `internal-auth` feature; rebuild with \
                 --features internal-auth"
                    .into(),
            );
        }
    }

    if !static_tokens {
        if !options.anonymous {
            eprintln!(
                "warning: no credentials configured; starting in anonymous read-only mode.\n\
                 Pass --client for a real OAuth2 grant, --jwks-url to delegate, --bl-token for a \
                 pre-shared secret, or --anonymous to silence this."
            );
        }
        return Ok(Arc::new(AnonymousAuth::new(options.token_url)));
    }

    let mut auth = StaticTokenAuth::new(options.token_url);
    if let Some(t) = options.bl_token {
        auth = auth.with_business_logic(t, ClientId::new("business-logic")?);
    }
    if let Some(t) = options.ven_token {
        auth = auth.with_ven(t, ClientId::new("ven")?);
    }
    if options.anonymous {
        auth = auth.allowing_anonymous();
    }
    Ok(Arc::new(auth))
}

/// The host a client should use, given what the server binds to.
///
/// A wildcard bind is not an address anybody can connect to, so it becomes `localhost` and the
/// operator is told to pass `--public-url` for anything else.
fn advertised_host(listen: &str) -> String {
    let (host, port) = match listen.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (listen, None),
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let host = match bare {
        "0.0.0.0" | "::" | "" => "localhost".to_string(),
        other if other.contains(':') => format!("[{other}]"),
        other => other.to_string(),
    };
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

/// What the operator asked for, once the safety rule has been applied.
enum Backend {
    Memory,
    #[cfg(feature = "sqlite")]
    Sqlite(String),
    #[cfg(feature = "postgres")]
    Postgres(String),
}

impl Backend {
    async fn resolve(self) -> Result<SharedStorage, Box<dyn std::error::Error>> {
        match self {
            Backend::Memory => Ok(MemoryStorage::shared()),
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(path) => {
                let store = openadr::vtn::store::SqliteStorage::shared(&path).await?;
                tracing::info!(%path, "using SQLite storage");
                Ok(store)
            }
            #[cfg(feature = "postgres")]
            Backend::Postgres(url) => {
                let store = openadr::vtn::store::PostgresStorage::shared(&url).await?;
                // The URL carries a password. Log that Postgres is in use, never with what.
                tracing::info!("using PostgreSQL storage");
                Ok(store)
            }
        }
    }
}

/// Whether a `--database` value names a PostgreSQL server rather than a file.
fn is_postgres_url(value: &str) -> bool {
    value.starts_with("postgres://") || value.starts_with("postgresql://")
}

/// Refuse the combination that loses data silently.
///
/// In-memory storage on a loopback address is a development server. On a public address it is a
/// VTN that forgets every programme when it restarts, and nothing in the protocol tells a VEN that
/// happened — so it takes an explicit `--ephemeral`.
fn resolve_storage(
    database: Option<&str>,
    listen: &str,
    ephemeral: bool,
) -> Result<Backend, Box<dyn std::error::Error>> {
    if let Some(target) = database {
        if is_postgres_url(target) {
            #[cfg(feature = "postgres")]
            return Ok(Backend::Postgres(target.to_string()));
            #[cfg(not(feature = "postgres"))]
            return Err(
                "--database with a postgres:// URL needs the `postgres` feature; \
                 rebuild with --features postgres"
                    .into(),
            );
        }
        #[cfg(feature = "sqlite")]
        return Ok(Backend::Sqlite(target.to_string()));
        #[cfg(not(feature = "sqlite"))]
        return Err(format!(
            "--database {target} needs the `sqlite` feature; rebuild with --features sqlite"
        )
        .into());
    }

    if !ephemeral && !is_loopback(listen) {
        return Err(format!(
            "refusing to serve {listen} from in-memory storage: everything is lost on restart.\n\nPass --database <PATH|postgres://…> for durable storage, or --ephemeral if that is really what you want."
        )
        .into());
    }
    if ephemeral {
        tracing::warn!("in-memory storage: all state is lost on restart");
    }
    Ok(Backend::Memory)
}

/// Read a retention period: `90d`, `12h`, `30m`, `45s`, or a bare number of seconds.
///
/// Deliberately *not* ISO 8601. `P90D` is the specification's spelling for a duration on the wire,
/// and this is an operator typing into a systemd unit — where every other tool in the stack accepts
/// `90d`. A flag that took `P90D` would be the wire format leaking into the command line.
fn parse_duration(spec: &str) -> Result<std::time::Duration, String> {
    let spec = spec.trim();
    let (digits, multiplier) = match spec.as_bytes().last() {
        Some(b'd') => (&spec[..spec.len() - 1], 86_400),
        Some(b'h') => (&spec[..spec.len() - 1], 3_600),
        Some(b'm') => (&spec[..spec.len() - 1], 60),
        Some(b's') => (&spec[..spec.len() - 1], 1),
        _ => (spec, 1),
    };
    let count: u64 = digits
        .parse()
        .map_err(|_| format!("{spec:?} is not a duration; try 90d, 12h, 30m or 3600s"))?;
    if count == 0 {
        // Zero would delete every report the instant it was filed, which is never what somebody
        // meant to type and is unrecoverable.
        return Err(
            "a retention period of zero would delete every report as it arrives; omit \
                    --report-retention to keep reports for ever"
                .to_string(),
        );
    }
    count
        .checked_mul(multiplier)
        .map(std::time::Duration::from_secs)
        .ok_or_else(|| format!("{spec:?} is longer than this program can represent"))
}

/// Whether a bind address is reachable only from this host.
fn is_loopback(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Start the mDNS advertisement for a running VTN.
///
/// The instance name, the hostname and the port are the three things a browser needs; everything
/// else is the TXT record `VtnService` renders, which is tested without a network.
#[cfg(feature = "mdns")]
fn advertise(
    listen: &str,
    base_path: &str,
    name: Option<String>,
    host: Option<String>,
    programs: Vec<String>,
    requires_auth: bool,
    tls: bool,
) -> Result<openadr::discovery::Advertisement, Box<dyn std::error::Error>> {
    use openadr::discovery::{Advertisement, VtnService};

    let instance = name.unwrap_or_else(|| "openadr-vtn".to_string());
    let port = listen
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .ok_or_else(|| format!("--listen {listen:?} names no port to advertise"))?;

    let mut service = VtnService::new(&instance, port)
        .with_base_path(base_path)
        .with_program_names(programs)
        .requiring_auth(requires_auth);
    if tls {
        service = service.with_tls();
    }
    if let Some(host) = host {
        service = service.with_hostname(host);
    }
    // `openapi_url` is one of the keys the record is defined to carry `[Def §Discovery]`, and the
    // VTN serves the document it names. Derived from `local_url` rather than restated, so a change
    // of scheme, host or base path moves both.
    let openapi_url = service.openapi_url();
    Ok(Advertisement::start(
        &service.with_openapi_url(openapi_url),
    )?)
}

#[cfg(test)]
mod tests {
    use super::{is_loopback, is_postgres_url};

    #[test]
    fn a_retention_period_reads_the_way_an_operator_writes_one() {
        use std::time::Duration;
        assert_eq!(
            super::parse_duration("90d").unwrap(),
            Duration::from_secs(90 * 86_400)
        );
        assert_eq!(
            super::parse_duration("12h").unwrap(),
            Duration::from_secs(12 * 3_600)
        );
        assert_eq!(
            super::parse_duration("30m").unwrap(),
            Duration::from_secs(1_800)
        );
        assert_eq!(
            super::parse_duration("45s").unwrap(),
            Duration::from_secs(45)
        );
        assert_eq!(
            super::parse_duration(" 3600 ").unwrap(),
            Duration::from_secs(3_600)
        );

        // Zero is refused rather than honoured: it would delete every report as it arrived, it is
        // never what anyone meant, and it cannot be undone.
        assert!(super::parse_duration("0").is_err());
        assert!(super::parse_duration("0d").is_err());
        // And so is anything that is not a period at all, including the wire format's own spelling
        // — which would otherwise parse as a bare number and mean something else entirely.
        assert!(super::parse_duration("P90D").is_err());
        assert!(super::parse_duration("").is_err());
        assert!(super::parse_duration("-1d").is_err());
        assert!(super::parse_duration("99999999999999999999d").is_err());
    }

    #[test]
    fn a_postgres_url_is_not_mistaken_for_a_file_path() {
        for url in [
            "postgres://user:pw@db.internal/openadr",
            "postgresql://localhost/openadr",
        ] {
            assert!(is_postgres_url(url), "{url}");
        }
        for path in [
            "/var/lib/openadr/openadr.sqlite",
            "openadr.sqlite",
            "sqlite:openadr.db",
            // Close enough to catch a prefix check written the lazy way.
            "postgres-backup.sqlite",
        ] {
            assert!(!is_postgres_url(path), "{path}");
        }
    }

    #[test]
    fn a_wildcard_bind_is_not_advertised_as_a_url() {
        // `/auth/server` hands a client a URL to connect to. `0.0.0.0` means "every interface on
        // *my* machine", so sending it to a client sends the client to itself.
        assert_eq!(super::advertised_host("0.0.0.0:3000"), "localhost:3000");
        assert_eq!(super::advertised_host("[::]:3000"), "localhost:3000");
        assert_eq!(super::advertised_host("10.0.0.5:3000"), "10.0.0.5:3000");
        assert_eq!(
            super::advertised_host("vtn.example.com:443"),
            "vtn.example.com:443"
        );
        assert_eq!(super::advertised_host("[::1]:3000"), "[::1]:3000");
    }

    #[test]
    fn loopback_addresses_are_recognised() {
        for addr in [
            "127.0.0.1:3000",
            "localhost:3000",
            "[::1]:3000",
            "127.0.0.1",
        ] {
            assert!(is_loopback(addr), "{addr} should be loopback");
        }
        for addr in [
            "0.0.0.0:3000",
            "10.0.0.5:3000",
            "[::]:3000",
            "example.com:80",
        ] {
            assert!(!is_loopback(addr), "{addr} should not be loopback");
        }
    }
}
