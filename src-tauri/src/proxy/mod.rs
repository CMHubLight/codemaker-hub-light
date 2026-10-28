//! 本地代理：三协议真实转发 + SSE 透传 + usage/cost 落库（P2）。
//!
//! - 只监听回环地址，端口回退逻辑见 [`server`]；
//! - 凭据由 [`credentials::CredentialProvider`] 提供（生产实现读 `AuthService`）；
//! - 上游 URL/头规则见 [`forward`]，SSE 帧扫描见 [`sse_frames`]（`tee`/`translate` 共用），
//!   落库见 [`recorder`]；
//! - token 只在内存中用于注入上游请求头，绝不写入日志/数据库/事件。

pub mod credentials;
pub mod forward;
pub mod host_guard;
pub mod policy;
pub mod recorder;
pub mod routes;
pub mod server;
pub mod sse_frames;
pub mod tee;
pub mod translate;
pub mod usage_parser;

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod perf_tests;

pub use credentials::{AuthServiceCredentials, CredentialProvider};
pub use policy::ProxyPolicy;
pub use server::{ProxyError, ProxyServer, ProxyStatus, RuntimeInfo};
