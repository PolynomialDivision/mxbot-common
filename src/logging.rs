//! Tracing setup shared by all bots.

use tracing_subscriber::EnvFilter;

/// Install the fmt subscriber.
///
/// matrix-sdk logs full sync bodies and crypto internals at info/debug; at
/// that volume it rotates the bot's own (much rarer) logs out of a capped
/// container log. By default the bot crate logs at `info` and the SDK at
/// `warn`. `RUST_LOG` overrides this entirely.
///
/// `crate_name` is the bot's crate name with underscores, e.g. `quiz_bot`.
pub fn init(crate_name: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "{crate_name}=info,mxbot_common=info,matrix_sdk=warn,matrix_sdk_crypto=warn,\
             matrix_sdk_base=warn,warn"
        ))
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
