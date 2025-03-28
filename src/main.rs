mod admin;
mod balancer;
mod config;
mod health;
mod rpc;
mod websocket;

use crate::{
    admin::listener::listen_for_admin_requests,
    balancer::{
        accept_http::{
            accept_request,
            ConnectionParams,
            RequestChannels,
        },
        processing::CacheArgs,
    },
    config::{
        cache_setup::setup_data,
        cli_args::create_match,
        types::Settings,
    },
    health::{
        check::{
            dropped_listener,
            health_check,
        },
        head_cache::manage_cache,
        safe_block::{
            subscribe_to_new_heads,
            NamedBlocknumbers,
        },
    },
    rpc::types::Rpc,
    websocket::{
        client::ws_conn_manager,
        subscription_manager::subscription_dispatcher,
        types::{
            IncomingResponse,
            SubscriptionData,
            WsChannelErr,
            WsconnMessage,
        },
    },
};

use std::{
    collections::BTreeMap,
    println,
    sync::{
        Arc,
        RwLock,
    },
};

use tokio::{
    net::TcpListener,
    sync::{
        broadcast,
        mpsc,
        watch,
    },
};

use hyper::{
    server::conn::http1,
    service::service_fn,
};
use hyper_util_blutgang::rt::TokioIo;

// jeemalloc offers faster mallocs when dealing with lots of threads which is what we're doing
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Get all the cli args and set them
    let config = Arc::new(RwLock::new(Settings::new(create_match()).await));

    // Copy the configuration values we need
    let (addr, do_clear, do_health_check, admin_enabled, is_ws, health_check_ttl) = {
        let config_guard = config.read().unwrap();
        (
            config_guard.address,
            config_guard.do_clear,
            config_guard.health_check,
            config_guard.admin.enabled,
            config_guard.is_ws,
            config_guard.health_check_ttl,
        )
    };

    // Make the list a rwlock
    let rpc_list_rwlock = Arc::new(RwLock::new(config.read().unwrap().rpc_list.clone()));

    // Create/Open sled DB
    let cache = Arc::new(config.read().unwrap().sled_config.open().unwrap());

    // Cache for storing querries near the tip
    let head_cache = Arc::new(RwLock::new(BTreeMap::<u64, Vec<String>>::new()));

    // Clear database if specified
    if do_clear {
        cache.clear().unwrap();
        println!("\x1b[93mWrn:\x1b[0m All data cleared from the database.");
    }
    // Insert data about blutgang and our settings into the DB
    //
    // Print any relevant warnings about a misconfigured DB. Check docs for more
    setup_data(Arc::clone(&cache));

    // We create a TcpListener and bind it to 127.0.0.1:3000
    let listener = TcpListener::bind(addr).await?;
    println!("\x1b[35mInfo:\x1b[0m Bound to: {}", addr);

    let (blocknum_tx, blocknum_rx) = watch::channel(0);
    let (finalized_tx, finalized_rx) = watch::channel(0);

    let finalized_rx_arc = Arc::new(finalized_rx.clone());
    let rpc_poverty_list = Arc::new(RwLock::new(Vec::<Rpc>::new()));

    // Spawn a thread for the admin namespace if enabled
    if admin_enabled {
        let rpc_list_admin = Arc::clone(&rpc_list_rwlock);
        let poverty_list_admin = Arc::clone(&rpc_poverty_list);
        let cache_admin = Arc::clone(&cache);
        let config_admin = Arc::clone(&config);
        tokio::task::spawn(async move {
            println!("\x1b[35mInfo:\x1b[0m Admin namespace enabled, accepting admin methods at admin port");
            let _ = listen_for_admin_requests(
                rpc_list_admin,
                poverty_list_admin,
                cache_admin,
                config_admin,
            )
            .await;
        });
    }

    // Spawn a thread for the head cache
    let head_cache_clone = Arc::clone(&head_cache);
    let cache_clone = Arc::clone(&cache);
    let finalized_rxclone = Arc::clone(&finalized_rx_arc);
    tokio::task::spawn(async move {
        let _ = manage_cache(
            &head_cache_clone,
            blocknum_rx,
            finalized_rxclone,
            &cache_clone,
        )
        .await;
    });

    // Spawn a thread for the health check
    //
    // Also handle the finalized block tracking in this thread
    let named_blocknumbers = Arc::new(RwLock::new(NamedBlocknumbers::default()));

    if do_health_check {
        let poverty_list_health = Arc::clone(&rpc_poverty_list);
        let config_health = Arc::clone(&config);

        let rpc_list_health = Arc::clone(&rpc_list_rwlock);
        let named_blocknumbers_health = Arc::clone(&named_blocknumbers);

        tokio::task::spawn(async move {
            let _ = health_check(
                rpc_list_health,
                poverty_list_health,
                finalized_tx,
                &named_blocknumbers_health,
                &config_health,
            )
            .await;
        });
    }

    // WebSocket connection + health check setup. Only runs when every node has a WS endpoint.
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<WsconnMessage>();
    let (outgoing_tx, outgoing_rx) = broadcast::channel::<IncomingResponse>(2048);
    let sub_data = Arc::new(SubscriptionData::new());
    if is_ws {
        let (ws_error_tx, ws_error_rx) = mpsc::unbounded_channel::<WsChannelErr>();

        let rpc_list_ws = Arc::clone(&rpc_list_rwlock);
        // TODO: make this more ergonomic
        let ws_handle = Arc::new(RwLock::new(Vec::<
            Option<mpsc::UnboundedSender<serde_json::Value>>,
        >::new()));
        let outgoing_rx_ws = outgoing_rx.resubscribe();
        let incoming_tx_ws = incoming_tx.clone();
        let ws_error_tx_ws = ws_error_tx.clone();

        let sub_dispatcher = Arc::clone(&sub_data);

        tokio::task::spawn(async move {
            tokio::task::spawn(async move {
                let _ =
                    subscription_dispatcher(outgoing_rx_ws, incoming_tx_ws, sub_dispatcher).await;
            });

            let _ = ws_conn_manager(
                rpc_list_ws,
                ws_handle,
                incoming_rx,
                outgoing_tx,
                ws_error_tx_ws,
            )
            .await;
        });

        if do_health_check {
            let dropped_rpc = Arc::clone(&rpc_list_rwlock);
            let dropped_povrty = Arc::clone(&rpc_poverty_list);
            let dropped_inc = incoming_tx.clone();
            let dropped_rx = outgoing_rx.resubscribe();
            let dropped_sub_data = Arc::clone(&sub_data);

            tokio::task::spawn(async move {
                dropped_listener(
                    dropped_rpc,
                    dropped_povrty,
                    ws_error_rx,
                    dropped_inc,
                    dropped_rx,
                    dropped_sub_data,
                )
                .await
            });

            let heads_inc = incoming_tx.clone();
            let heads_rx = outgoing_rx.resubscribe();
            let heads_sub_data = sub_data.clone();

            let cache_args = CacheArgs {
                finalized_rx: finalized_rx.clone(),
                named_numbers: named_blocknumbers.clone(),
                cache: cache.clone(),
                head_cache: head_cache.clone(),
            };

            tokio::task::spawn(async move {
                subscribe_to_new_heads(
                    heads_inc,
                    heads_rx,
                    blocknum_tx,
                    heads_sub_data,
                    cache_args,
                    health_check_ttl,
                )
                .await;
            });
        }
    }

    // We start a loop to continuously accept incoming connections
    loop {
        let (stream, socketaddr) = listener.accept().await?;
        println!("\x1b[35mInfo:\x1b[0m Connection from: {}", socketaddr);

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);

        let channels = RequestChannels::new(
            finalized_rx_arc.clone(),
            incoming_tx.clone(),
            outgoing_rx.resubscribe(),
        );

        let connection_params = ConnectionParams::new(
            &rpc_list_rwlock,
            channels,
            &named_blocknumbers,
            &head_cache,
            &sub_data,
            &cache,
            &config,
        );

        // Spawn a tokio task to serve multiple connections concurrently
        tokio::task::spawn(async move {
            accept!(io, connection_params.clone());
        });
    }
}
