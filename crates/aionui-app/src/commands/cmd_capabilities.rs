//! Top-level agent-readable capability index for the `centaurai-core` binary.

use std::io::{self, Write};
use std::process::ExitCode;

use serde_json::{Value, json};

const RUNTIME_ENV: [&str; 4] = [
    "CENTAURAI_CORE_HELPER_BIN",
    "CENTAURAI_CORE_BASE_URL",
    "CENTAURAI_CORE_CONVERSATION_ID",
    "CENTAURAI_CORE_USER_ID",
];
const LEGACY_RUNTIME_ENV: [&str; 4] = [
    "AIONUI_HELPER_BIN",
    "AIONUI_BASE_URL",
    "AIONUI_CONVERSATION_ID",
    "AIONUI_USER_ID",
];

pub(crate) fn run_capabilities() -> ExitCode {
    match print_envelope(data()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => {
            eprintln!("CAPABILITIES_STDOUT_WRITE_FAILED command=\"capabilities\": failed to write JSON output");
            ExitCode::from(1)
        }
    }
}

fn data() -> Value {
    json!({
        "schema_version": 1,
        "contract": "agent-facing-centaurai-core-cli",
        "stability": "stable",
        "entrypoint": "centaurai-core capabilities",
        "purpose": "Top-level index for agent-facing CentaurAI Core CLI domains.",
        "output": {
            "stdout": "JSON envelope",
            "stderr": "single stable ..._FAILED error line when output cannot be written",
            "success_shape": {
                "success": true,
                "data": {},
                "meta": {
                    "schema_version": 1
                }
            }
        },
        "runtime_context": {
            "primary": "CENTAURAI_CORE_CONVERSATION_ID",
            "environment": RUNTIME_ENV,
            "legacy_environment": LEGACY_RUNTIME_ENV,
            "selectors": {
                "conversation_id": {
                    "current": "resolve from CENTAURAI_CORE_CONVERSATION_ID"
                },
                "assistant_id": {
                    "current": "resolve via current conversation"
                },
                "user_id": {
                    "current": "resolve from CENTAURAI_CORE_USER_ID"
                }
            }
        },
        "input": {
            "default_mode": "stdin_json",
            "business_flags": false,
            "domain_contracts": "Use each domain's capabilities command for exact stdin fields and safety metadata."
        },
        "domains": [
            {
                "name": "config",
                "mode": "read-write",
                "description": "Manage CentaurAI configuration: assistants, assistant rules, skills, MCP servers, providers, settings, agents, and scheduled tasks.",
                "contract": "agent-facing-config-cli",
                "contract_command": "config capabilities",
                "invocation": "centaurai-core config capabilities",
                "runtime_required": ["CENTAURAI_CORE_BASE_URL", "CENTAURAI_CORE_CONVERSATION_ID", "CENTAURAI_CORE_USER_ID"],
                "safety": {
                    "can_write": true,
                    "read_before_write": true,
                    "redacted_by_default": true
                }
            },
            {
                "name": "diagnose",
                "mode": "read-only",
                "description": "Diagnose a running CentaurAI installation: backend health, conversations, provider health, MCP, cron, teams, logs, and controlled GET reads.",
                "contract": "agent-facing-diagnose-cli",
                "contract_command": "diagnose capabilities",
                "invocation": "centaurai-core diagnose capabilities",
                "runtime_required": ["CENTAURAI_CORE_BASE_URL", "CENTAURAI_CORE_CONVERSATION_ID", "CENTAURAI_CORE_USER_ID"],
                "optional_runtime": ["CENTAURAI_CORE_LOG_DIR"],
                "safety": {
                    "can_write": false,
                    "read_only": true,
                    "redacted_by_default": true,
                    "escape_hatch": "diagnose http get"
                }
            }
        ],
        "non_agent_subcommands": [
            {
                "name": "doctor",
                "description": "Human/developer self-check for agent backend availability."
            },
            {
                "name": "mcp-bridge",
                "description": "Internal stdio to TCP bridge for team MCP."
            },
            {
                "name": "mcp-team-stdio",
                "description": "Internal team MCP stdio server."
            },
            {
                "name": "prepare-managed-resources",
                "description": "Packaging helper for managed runtime resources."
            }
        ]
    })
}

fn print_envelope(data: Value) -> Result<(), ()> {
    let rendered = serde_json::to_string_pretty(&json!({
        "success": true,
        "data": data,
        "meta": {
            "schema_version": 1
        }
    }))
    .map_err(|_| ())?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(rendered.as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|_| ())
}
