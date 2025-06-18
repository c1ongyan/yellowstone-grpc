//!
//! Version information.
//!
use {serde::Serialize, std::fmt, std::env};

/// Holds detailed version information for the plugin and its dependencies.
/// This data is captured at compile time from environment variables.
#[derive(Debug, Serialize, Clone, Copy)]
pub struct Version {
    /// The name of the cargo package.
    pub package: &'static str,
    /// The version of the cargo package.
    pub version: &'static str,
    /// The version of the `yellowstone-grpc-proto` crate.
    pub proto: &'static str,
    /// The version of the Solana SDK used.
    pub solana: &'static str,
    /// The git commit hash of the build.
    pub git: &'static str,
    /// The version of the Rust compiler used.
    pub rustc: &'static str,
    /// The timestamp of the build.
    pub buildts: &'static str,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "solana-geyser-grpc={}, proto={}, solana={}, git={}, rustc={}, buildts={}",
            self.version, self.proto, self.solana, self.git, self.rustc, self.buildts
        )
    }
}

/// A constant holding the compile-time version information.
pub const VERSION: Version = Version {
    package: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    proto: env!("YELLOWSTONE_GRPC_PROTO_VERSION"),
    solana: env!("SOLANA_SDK_VERSION"),
    git: env!("GIT_VERSION"),
    rustc: env!("VERGEN_RUSTC_SEMVER"),
    buildts: env!("VERGEN_BUILD_TIMESTAMP"),
};

/// Additional runtime information for the gRPC version response.
#[derive(Debug, Serialize)]
pub struct GrpcVersionInfoExtra {
    /// The hostname of the machine running the plugin.
    hostname: Option<String>,
}

/// A struct that combines compile-time and runtime version information,
/// primarily for the `GetVersion` gRPC method.
#[derive(Debug, Serialize)]
pub struct GrpcVersionInfo {
    version: Version,
    extra: GrpcVersionInfoExtra,
}

impl GrpcVersionInfo {
    /// Gets the gRPC version information.
    pub fn get() -> Self {
        Self::default()
    }
}

impl Default for GrpcVersionInfo {
    fn default() -> Self {
        Self {
            version: VERSION,
            extra: GrpcVersionInfoExtra {
                hostname: hostname::get()
                    .ok()
                    .and_then(|name| name.into_string().ok()),
            },
        }
    }
}

impl fmt::Display for GrpcVersionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(hostname) = &self.extra.hostname {
            write!(f, "{}, hostname={}", self.version, hostname)
        } else {
            write!(f, "{}", self.version)
        }
    }
}
