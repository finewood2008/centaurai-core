//! CLI argument definitions for the `centaurai-core` binary.
//!
//! Kept separate from `main.rs` to isolate the clap surface (struct + enum +
//! attribute soup) from the runtime entry point. Visibility is `pub(crate)`
//! because only `main.rs` consumes it.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "centaurai-core", about = "CentaurAI Core Server", version)]
pub(crate) struct Cli {
    /// Host address to listen on.
    #[arg(long, default_value_t = String::from(aionui_common::constants::DEFAULT_HOST))]
    pub host: String,

    /// Port number to listen on.
    #[arg(long, default_value_t = aionui_common::constants::DEFAULT_PORT)]
    pub port: u16,

    /// Data directory for database and file storage.
    #[arg(long, default_value = "data")]
    pub data_dir: PathBuf,

    /// Parent process ID used to terminate the backend when the desktop app dies.
    #[arg(long)]
    pub parent_pid: Option<u32>,

    /// Working directory for conversation workspaces.
    /// Falls back to AIONUI_WORK_DIR env, then to data-dir.
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// Host application version used for extension engine compatibility.
    #[arg(long, default_value_t = env!("CARGO_PKG_VERSION").to_string())]
    pub app_version: String,

    /// Run in local embedded mode (skip authentication, use system_default_user).
    #[arg(long)]
    pub local: bool,

    /// Directory for log files. Defaults to {data-dir}/logs/.
    #[arg(long)]
    pub log_dir: Option<PathBuf>,

    /// Log level filter (e.g. "info", "debug", "info,aionui_mcp=trace").
    #[arg(long)]
    pub log_level: Option<String>,

    /// Dump prompt diagnostics to {data-dir}/prompt-dumps.
    #[arg(long)]
    pub dump_prompts: bool,

    /// Explicitly back up a corruption-like local database and create a fresh database during startup.
    #[arg(long)]
    pub recover_corrupted_database: bool,

    /// Managed runtime resource source selection.
    #[arg(long, value_enum, default_value_t = ManagedResourcesModeArg::Download)]
    pub managed_resources_mode: ManagedResourcesModeArg,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedResourcesModeArg {
    Bundled,
    Download,
}

impl From<ManagedResourcesModeArg> for aionui_runtime::ManagedResourcesMode {
    fn from(value: ManagedResourcesModeArg) -> Self {
        match value {
            ManagedResourcesModeArg::Bundled => Self::Bundled,
            ManagedResourcesModeArg::Download => Self::Download,
        }
    }
}

// `Mcp` prefix is load-bearing on Mcp* variants — clap derives kebab-case
// subcommand names (`mcp-bridge`, `mcp-team-stdio`)
// that external callers (ACP agent CLI, team MCP bridge spec) depend on
// verbatim.
#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Print the top-level agent-facing CLI capability index.
    Capabilities,
    /// Agent-facing automation CLI for AionUi configuration.
    Config(ConfigArgs),
    /// Agent-facing read-only troubleshooting CLI for AionUi diagnosis.
    Diagnose(DiagnoseArgs),
    /// Stdio ↔ TCP bridge for the team MCP server (spawned by the ACP agent CLI).
    McpBridge,
    /// MCP stdio server for team tools (spawned by the ACP agent CLI).
    McpTeamStdio,
    /// Self-check: hydrate the agent registry, probe every CLI on `$PATH`,
    /// and print a per-agent availability table. Useful when the user
    /// reports "no agent works" — running this from the same shell the
    /// app launched from confirms whether each backend is detectable
    /// before involving server logs.
    Doctor,
    /// Prepare current-platform managed runtime resources under a bundle output root.
    PrepareManagedResources(PrepareManagedResourcesArgs),
}

impl Command {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Capabilities => "capabilities",
            Self::Config(_) => "config",
            Self::Diagnose(_) => "diagnose",
            Self::McpBridge => "mcp-bridge",
            Self::McpTeamStdio => "mcp-team-stdio",
            Self::Doctor => "doctor",
            Self::PrepareManagedResources(_) => "prepare-managed-resources",
        }
    }

    pub(crate) fn need_runtime(&self) -> bool {
        matches!(self, Self::Doctor | Self::PrepareManagedResources(_))
    }
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseArgs {
    #[command(subcommand)]
    pub command: DiagnoseCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseCommand {
    /// Print the agent-readable diagnose CLI capability contract.
    Capabilities,
    /// Print the current agent runtime context.
    Context,
    /// Read backend health.
    Health,
    /// Read a cross-domain diagnostic snapshot.
    Overview,
    /// Inspect conversation state and messages.
    Conversations(DiagnoseConversationsArgs),
    /// Inspect provider health summary.
    Providers(DiagnoseProvidersArgs),
    /// Inspect MCP server summary.
    Mcp(DiagnoseMcpArgs),
    /// Inspect scheduled task summary.
    Cron(DiagnoseCronArgs),
    /// Inspect team summary.
    Teams(DiagnoseTeamsArgs),
    /// Read centaurai-core logs.
    Logs(DiagnoseLogsArgs),
    /// Controlled HTTP read escape hatch.
    Http(DiagnoseHttpArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseConversationsArgs {
    #[command(subcommand)]
    pub command: DiagnoseConversationsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseConversationsCommand {
    List,
    Get,
    Messages,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseProvidersArgs {
    #[command(subcommand)]
    pub command: DiagnoseSummaryCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseMcpArgs {
    #[command(subcommand)]
    pub command: DiagnoseSummaryCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseCronArgs {
    #[command(subcommand)]
    pub command: DiagnoseSummaryCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseTeamsArgs {
    #[command(subcommand)]
    pub command: DiagnoseSummaryCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseSummaryCommand {
    Summary,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseLogsArgs {
    #[command(subcommand)]
    pub command: DiagnoseLogsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseLogsCommand {
    Tail,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DiagnoseHttpArgs {
    #[command(subcommand)]
    pub command: DiagnoseHttpCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum DiagnoseHttpCommand {
    Get,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCommand {
    /// Print the agent-readable config CLI capability contract.
    Capabilities,
    /// Print the current agent runtime context.
    Context,
    /// Manage assistants and assistant-owned behavior.
    Assistants(ConfigAssistantsArgs),
    /// Manage AionUi skills.
    Skills(ConfigSkillsArgs),
    /// Manage MCP servers and OAuth state.
    Mcp(ConfigMcpArgs),
    /// Manage model providers.
    Providers(ConfigProvidersArgs),
    /// Manage backend and client settings.
    Settings(ConfigSettingsArgs),
    /// Manage agent catalog and custom agents.
    Agents(ConfigAgentsArgs),
    /// Manage scheduled tasks.
    Cron(ConfigCronArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAssistantsArgs {
    #[command(subcommand)]
    pub command: ConfigAssistantsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigAssistantsCommand {
    List,
    Get,
    Create,
    Update,
    Delete,
    Import,
    State,
    Rule(ConfigAssistantRuleArgs),
    Skill(ConfigAssistantSkillArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAssistantRuleArgs {
    #[command(subcommand)]
    pub command: ConfigAssistantTextCommand,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAssistantSkillArgs {
    #[command(subcommand)]
    pub command: ConfigAssistantTextCommand,
}

#[derive(Subcommand, Debug, Clone, Copy)]
pub(crate) enum ConfigAssistantTextCommand {
    Read,
    Write,
    Delete,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSkillsArgs {
    #[command(subcommand)]
    pub command: ConfigSkillsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSkillsCommand {
    List,
    Info,
    Paths,
    Import,
    Delete,
    Scan,
    ExternalPaths(ConfigSkillsExternalPathsArgs),
    Market(ConfigSkillsMarketArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSkillsExternalPathsArgs {
    #[command(subcommand)]
    pub command: ConfigSkillsExternalPathsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSkillsExternalPathsCommand {
    List,
    Add,
    Remove,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSkillsMarketArgs {
    #[command(subcommand)]
    pub command: ConfigSkillsMarketCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSkillsMarketCommand {
    Enable,
    Disable,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigMcpArgs {
    #[command(subcommand)]
    pub command: ConfigMcpCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigMcpCommand {
    Servers(ConfigMcpServersArgs),
    TestConnection,
    AgentConfigs,
    Oauth(ConfigMcpOauthArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigMcpServersArgs {
    #[command(subcommand)]
    pub command: ConfigMcpServersCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigMcpServersCommand {
    List,
    Get,
    Create,
    Update,
    Delete,
    Toggle,
    Import,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigMcpOauthArgs {
    #[command(subcommand)]
    pub command: ConfigMcpOauthCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigMcpOauthCommand {
    CheckStatus,
    Login,
    Logout,
    Authenticated,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigProvidersArgs {
    #[command(subcommand)]
    pub command: ConfigProvidersCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigProvidersCommand {
    List,
    Create,
    Update,
    Delete,
    DetectProtocol,
    FetchModels,
    Models(ConfigProviderModelsArgs),
    HealthCheck,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigProviderModelsArgs {
    #[command(subcommand)]
    pub command: ConfigProviderModelsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigProviderModelsCommand {
    Fetch,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSettingsArgs {
    #[command(subcommand)]
    pub command: ConfigSettingsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSettingsCommand {
    Get,
    Patch,
    Client(ConfigSettingsClientArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigSettingsClientArgs {
    #[command(subcommand)]
    pub command: ConfigSettingsClientCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigSettingsClientCommand {
    Get,
    Put,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAgentsArgs {
    #[command(subcommand)]
    pub command: ConfigAgentsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigAgentsCommand {
    List,
    Enable,
    Overrides(ConfigAgentOverridesArgs),
    Custom(ConfigAgentCustomArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAgentOverridesArgs {
    #[command(subcommand)]
    pub command: ConfigAgentOverridesCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigAgentOverridesCommand {
    Get,
    Set,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigAgentCustomArgs {
    #[command(subcommand)]
    pub command: ConfigAgentCustomCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigAgentCustomCommand {
    Create,
    Update,
    Delete,
    TryConnect,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigCronArgs {
    #[command(subcommand)]
    pub command: ConfigCronCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCronCommand {
    Jobs(ConfigCronJobsArgs),
    Current(ConfigCronCurrentArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigCronJobsArgs {
    #[command(subcommand)]
    pub command: ConfigCronJobsCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCronJobsCommand {
    List,
    Get,
    Create,
    Update,
    Delete,
    Run,
    Skill(ConfigCronJobSkillArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigCronJobSkillArgs {
    #[command(subcommand)]
    pub command: ConfigCronJobSkillCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCronJobSkillCommand {
    Get,
    Save,
    Delete,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ConfigCronCurrentArgs {
    #[command(subcommand)]
    pub command: ConfigCronCurrentCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum ConfigCronCurrentCommand {
    List,
    Create,
    Update,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct PrepareManagedResourcesArgs {
    /// Bundle output root. Aioncore writes the managed resources under
    /// `<bundle-out>/{node,acp}/...` for packaging.
    #[arg(long)]
    pub bundle_out: PathBuf,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;
    use clap::error::ErrorKind;

    use super::{Cli, Command, ConfigArgs, ConfigCommand, ManagedResourcesModeArg, PrepareManagedResourcesArgs};

    #[test]
    fn long_version_flag_uses_workspace_package_version() {
        let result = Cli::try_parse_from(["centaurai-core", "--version"]);
        let err = match result {
            Ok(_) => panic!("expected --version to exit through clap DisplayVersion"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert!(
            rendered.contains("centaurai-core"),
            "version output should contain binary name, got: {rendered:?}"
        );
        assert!(
            rendered.contains(env!("CARGO_PKG_VERSION")),
            "version output should contain package version {}, got: {rendered:?}",
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn short_version_flag_uses_workspace_package_version() {
        let result = Cli::try_parse_from(["centaurai-core", "-V"]);
        let err = match result {
            Ok(_) => panic!("expected -V to exit through clap DisplayVersion"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert!(
            rendered.contains("centaurai-core"),
            "version output should contain binary name, got: {rendered:?}"
        );
        assert!(
            rendered.contains(env!("CARGO_PKG_VERSION")),
            "version output should contain package version {}, got: {rendered:?}",
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn prepare_managed_resources_accepts_bundle_out() {
        let cli = Cli::parse_from([
            "centaurai-core",
            "prepare-managed-resources",
            "--bundle-out",
            "/tmp/centaurai-core-bundle",
        ]);

        match cli.command {
            Some(Command::PrepareManagedResources(args)) => {
                assert_eq!(args.bundle_out, std::path::Path::new("/tmp/centaurai-core-bundle"));
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn managed_resources_mode_defaults_to_download() {
        let cli = Cli::parse_from(["centaurai-core"]);
        assert_eq!(cli.managed_resources_mode, ManagedResourcesModeArg::Download);
    }

    #[test]
    fn managed_resources_mode_accepts_download() {
        let cli = Cli::parse_from(["centaurai-core", "--managed-resources-mode", "download"]);
        assert_eq!(cli.managed_resources_mode, ManagedResourcesModeArg::Download);
    }

    #[test]
    fn parent_pid_accepts_positive_integer() {
        let cli = Cli::parse_from(["centaurai-core", "--parent-pid", "4242"]);
        assert_eq!(cli.parent_pid, Some(4242));
    }

    #[test]
    fn dump_prompts_defaults_to_false() {
        let cli = Cli::parse_from(["centaurai-core"]);
        assert!(!cli.dump_prompts);
    }

    #[test]
    fn dump_prompts_accepts_flag() {
        let cli = Cli::parse_from(["centaurai-core", "--dump-prompts"]);
        assert!(cli.dump_prompts);
    }

    #[test]
    fn recover_corrupted_database_flag_defaults_to_false() {
        let cli = Cli::parse_from(["centaurai-core"]);
        assert!(!cli.recover_corrupted_database);
    }

    #[test]
    fn recover_corrupted_database_flag_is_accepted() {
        let cli = Cli::parse_from(["centaurai-core", "--recover-corrupted-database"]);
        assert!(cli.recover_corrupted_database);
    }

    #[test]
    fn command_as_str_returns_clap_subcommand_names() {
        let prepare_args = PrepareManagedResourcesArgs {
            bundle_out: PathBuf::from("/tmp/centaurai-core-bundle"),
        };

        let cases = [
            (
                Command::Config(ConfigArgs {
                    command: ConfigCommand::Context,
                }),
                "config",
            ),
            (Command::McpBridge, "mcp-bridge"),
            (Command::McpTeamStdio, "mcp-team-stdio"),
            (Command::Doctor, "doctor"),
            (
                Command::PrepareManagedResources(prepare_args),
                "prepare-managed-resources",
            ),
        ];

        for (command, expected) in cases {
            assert_eq!(command.as_str(), expected);
        }
    }

    #[test]
    fn config_cli_accepts_agent_facing_design_command_paths() {
        let commands: &[&[&str]] = &[
            &["centaurai-core", "config", "capabilities"],
            &["centaurai-core", "config", "context"],
            &["centaurai-core", "config", "assistants", "list"],
            &["centaurai-core", "config", "assistants", "get"],
            &["centaurai-core", "config", "assistants", "create"],
            &["centaurai-core", "config", "assistants", "update"],
            &["centaurai-core", "config", "assistants", "delete"],
            &["centaurai-core", "config", "assistants", "import"],
            &["centaurai-core", "config", "assistants", "state"],
            &["centaurai-core", "config", "assistants", "rule", "read"],
            &["centaurai-core", "config", "assistants", "rule", "write"],
            &["centaurai-core", "config", "assistants", "rule", "delete"],
            &["centaurai-core", "config", "assistants", "skill", "read"],
            &["centaurai-core", "config", "assistants", "skill", "write"],
            &["centaurai-core", "config", "assistants", "skill", "delete"],
            &["centaurai-core", "config", "skills", "list"],
            &["centaurai-core", "config", "skills", "info"],
            &["centaurai-core", "config", "skills", "paths"],
            &["centaurai-core", "config", "skills", "import"],
            &["centaurai-core", "config", "skills", "delete"],
            &["centaurai-core", "config", "skills", "scan"],
            &["centaurai-core", "config", "mcp", "servers", "list"],
            &["centaurai-core", "config", "mcp", "servers", "get"],
            &["centaurai-core", "config", "mcp", "servers", "create"],
            &["centaurai-core", "config", "mcp", "servers", "update"],
            &["centaurai-core", "config", "mcp", "servers", "delete"],
            &["centaurai-core", "config", "mcp", "servers", "toggle"],
            &["centaurai-core", "config", "mcp", "servers", "import"],
            &["centaurai-core", "config", "mcp", "test-connection"],
            &["centaurai-core", "config", "mcp", "agent-configs"],
            &["centaurai-core", "config", "mcp", "oauth", "check-status"],
            &["centaurai-core", "config", "mcp", "oauth", "login"],
            &["centaurai-core", "config", "mcp", "oauth", "logout"],
            &["centaurai-core", "config", "mcp", "oauth", "authenticated"],
            &["centaurai-core", "config", "providers", "list"],
            &["centaurai-core", "config", "providers", "create"],
            &["centaurai-core", "config", "providers", "update"],
            &["centaurai-core", "config", "providers", "delete"],
            &["centaurai-core", "config", "providers", "detect-protocol"],
            &["centaurai-core", "config", "providers", "fetch-models"],
            &["centaurai-core", "config", "providers", "models", "fetch"],
            &["centaurai-core", "config", "providers", "health-check"],
            &["centaurai-core", "config", "settings", "get"],
            &["centaurai-core", "config", "settings", "patch"],
            &["centaurai-core", "config", "settings", "client", "get"],
            &["centaurai-core", "config", "settings", "client", "put"],
            &["centaurai-core", "config", "agents", "list"],
            &["centaurai-core", "config", "agents", "enable"],
            &["centaurai-core", "config", "agents", "overrides", "get"],
            &["centaurai-core", "config", "agents", "overrides", "set"],
            &["centaurai-core", "config", "agents", "custom", "create"],
            &["centaurai-core", "config", "agents", "custom", "update"],
            &["centaurai-core", "config", "agents", "custom", "delete"],
            &["centaurai-core", "config", "agents", "custom", "try-connect"],
            &["centaurai-core", "config", "cron", "jobs", "list"],
            &["centaurai-core", "config", "cron", "jobs", "get"],
            &["centaurai-core", "config", "cron", "jobs", "create"],
            &["centaurai-core", "config", "cron", "jobs", "update"],
            &["centaurai-core", "config", "cron", "jobs", "delete"],
            &["centaurai-core", "config", "cron", "jobs", "run"],
            &["centaurai-core", "config", "cron", "jobs", "skill", "get"],
            &["centaurai-core", "config", "cron", "jobs", "skill", "save"],
            &["centaurai-core", "config", "cron", "jobs", "skill", "delete"],
            &["centaurai-core", "config", "skills", "external-paths", "list"],
            &["centaurai-core", "config", "skills", "external-paths", "add"],
            &["centaurai-core", "config", "skills", "external-paths", "remove"],
            &["centaurai-core", "config", "skills", "market", "enable"],
            &["centaurai-core", "config", "skills", "market", "disable"],
        ];

        for command in commands {
            let result = Cli::try_parse_from(*command);
            assert!(result.is_ok(), "command should parse: {command:?}");
        }
    }

    #[test]
    fn prepare_managed_resources_requires_bundle_out() {
        let err = match Cli::try_parse_from(["centaurai-core", "prepare-managed-resources"]) {
            Ok(_) => panic!("prepare-managed-resources should require --bundle-out"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}
