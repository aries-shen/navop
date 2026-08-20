//! Composite extension manifest、native IPC host 与 Declarative UI 的薄适配层。
//!
//! 本 crate 不实现 provider，也不负责进程重启、通用 capability 授权或面板挂载；
//! 它只把 catalog 中已经校验的 IPC binding 转为单次 session 配置，在 spawn
//! 边界复核 command 权限和路径 containment，并在进程内 UI event/state 与稳定
//! wire DTO 之间做无副作用转换。

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use declarative_ui_demo::{ActionEvent, Runtime, RuntimeError, StateChange, StateOperation};
use extension_host::{NegotiationConfig, ProcessRpcSessionConfig, SpawnConfig};
use extension_protocol::declarative_ui::{UiActionRequest, UiStateOperation, UiStatePatch};
use extension_runtime::RegisteredIpcRuntimeBinding;
use gpui::Context;
use thiserror::Error;

pub mod activation;

pub use activation::{
    ActivationError, ActivationHandle, ActivationManager, ManagedRpcSession,
    RuntimeActivationState, SessionFactory, process_session_factory,
};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PluginAdapterError {
    #[error("unsupported IPC transport `{0}`")]
    UnsupportedTransport(String),
    #[error("IPC shutdown grace {0}ms exceeds the host limit")]
    ShutdownGraceOverflow(u64),
    #[error("IPC command is missing required permission `{0}`")]
    MissingSpawnPermission(String),
    #[error("IPC path `{path}` escapes allowed root `{allowed_root}`")]
    PathEscapesAllowedRoot {
        path: PathBuf,
        allowed_root: PathBuf,
    },
    #[error("failed to resolve IPC path `{path}`: {message}")]
    PathResolution { path: PathBuf, message: String },
}

/// 把静态 IPC binding 转为一次 native process session 的启动配置。
///
/// 本函数在真正生成 spawn 配置前再次校验 command 对应的精确 `spawn:` 权限和
/// symlink containment。其余 permissions 以及 `auto_restart`、
/// `max_restart_attempts` 属于更高层 activation manager 的策略。
pub fn process_session_config(
    binding: &RegisteredIpcRuntimeBinding,
    negotiation: NegotiationConfig,
) -> Result<ProcessRpcSessionConfig, PluginAdapterError> {
    if binding.transport_kind != "local_socket" {
        return Err(PluginAdapterError::UnsupportedTransport(
            binding.transport_kind.clone(),
        ));
    }
    validate_spawn_boundary(binding)?;
    let shutdown_grace_ms = u32::try_from(binding.shutdown_grace_ms)
        .map_err(|_| PluginAdapterError::ShutdownGraceOverflow(binding.shutdown_grace_ms))?;

    let mut spawn = SpawnConfig::new(binding.command.clone()).with_args(binding.args.clone());
    if let Some(working_dir) = &binding.working_dir {
        spawn = spawn
            .with_cwd(working_dir.clone())
            .with_cwd_root(binding.extension_root.clone());
    }
    if binding.required_spawn_permission.starts_with("spawn:./") {
        spawn = spawn.with_program_root(binding.extension_root.clone());
    } else {
        spawn = spawn.with_program_root("/usr/bin");
    }
    for (key, value) in &binding.env {
        spawn = spawn.with_env(key.clone(), value.clone());
    }
    if let Some(timeout_ms) = binding.connect_timeout_ms {
        spawn = spawn.with_ready_timeout(Duration::from_millis(timeout_ms));
    }

    Ok(ProcessRpcSessionConfig::new(spawn, negotiation)
        .with_shutdown_grace_ms(shutdown_grace_ms)
        .with_label(binding.runtime_key.clone()))
}

fn validate_spawn_boundary(
    binding: &RegisteredIpcRuntimeBinding,
) -> Result<(), PluginAdapterError> {
    if !binding
        .permissions
        .iter()
        .any(|permission| permission == &binding.required_spawn_permission)
    {
        return Err(PluginAdapterError::MissingSpawnPermission(
            binding.required_spawn_permission.clone(),
        ));
    }

    ensure_path_within(&binding.extension_root, &binding.extension_root)?;
    if let Some(working_dir) = &binding.working_dir {
        ensure_path_within(&binding.extension_root, working_dir)?;
    }
    if binding.required_spawn_permission.starts_with("spawn:./") {
        ensure_path_within(&binding.extension_root, &binding.command)?;
    } else {
        ensure_path_within(Path::new("/usr/bin"), &binding.command)?;
    }
    Ok(())
}

fn ensure_path_within(allowed_root: &Path, candidate: &Path) -> Result<(), PluginAdapterError> {
    let canonical_root =
        allowed_root
            .canonicalize()
            .map_err(|error| PluginAdapterError::PathResolution {
                path: allowed_root.to_path_buf(),
                message: error.to_string(),
            })?;
    let mut existing = candidate;
    while !existing.exists() {
        let Some(parent) = existing.parent() else {
            return Err(PluginAdapterError::PathResolution {
                path: candidate.to_path_buf(),
                message: "no existing ancestor".into(),
            });
        };
        existing = parent;
    }
    let canonical_existing =
        existing
            .canonicalize()
            .map_err(|error| PluginAdapterError::PathResolution {
                path: existing.to_path_buf(),
                message: error.to_string(),
            })?;
    if canonical_existing.starts_with(&canonical_root) {
        return Ok(());
    }
    Err(PluginAdapterError::PathEscapesAllowedRoot {
        path: candidate.to_path_buf(),
        allowed_root: allowed_root.to_path_buf(),
    })
}

/// 将进程内 Declarative UI action 转成跨进程请求。
pub fn ui_action_request(
    event: &ActionEvent,
    request_id: impl Into<String>,
    expected_revision: Option<u64>,
) -> UiActionRequest {
    UiActionRequest {
        request_id: request_id.into(),
        action: event.name().to_owned(),
        source_id: event.source_id().to_owned(),
        source_path: event.source_path().0.clone(),
        payload: event.payload().clone(),
        expected_revision,
    }
}

/// 将 provider 返回的 wire patch 转成进程内原子 state operations。
pub fn state_operations(patch: &UiStatePatch) -> Vec<StateOperation> {
    patch
        .operations
        .iter()
        .map(|operation| match operation {
            UiStateOperation::Set { key, value } => StateOperation::Set {
                key: key.clone(),
                value: value.clone(),
            },
            UiStateOperation::Remove { key } => StateOperation::Remove { key: key.clone() },
        })
        .collect()
}

/// 在 GPUI entity update 中应用 provider 返回的原子 state patch。
pub fn apply_ui_state_patch(
    runtime: &mut Runtime,
    patch: &UiStatePatch,
    cx: &mut Context<Runtime>,
) -> Result<Option<StateChange>, RuntimeError> {
    let operations = state_operations(patch);
    runtime.apply_external_patch(patch.expected_revision, &operations, cx)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
