# Home-Manager module for the `themed` daemon.
#
# Import it through the flake:
#   imports = [ inputs.themed.homeManagerModules.default ];
#   services.themed.enable = true;
#   services.themed.peers = [ "fezzik.example.ts.net:47100" ... ];
#
# `homeManagerModules.default` also fills in `services.themed.package` with this
# flake's per-system build; importing this file directly instead expects
# `pkgs.themed` (i.e. the flake's `overlays.default`).
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.themed;

  isLinux = pkgs.stdenv.hostPlatform.isLinux;
  isDarwin = pkgs.stdenv.hostPlatform.isDarwin;

  args =
    lib.optionals (cfg.selfName != null) [ "--self" cfg.selfName ]
    ++ lib.optionals (cfg.listen != null) [ "--listen" cfg.listen ]
    ++ lib.optionals (cfg.socket != null) [ "--socket" cfg.socket ]
    ++ lib.optionals (cfg.stateFile != null) [ "--state-file" cfg.stateFile ]
    ++ lib.optionals (cfg.reconcileCmd != null) [ "--reconcile-cmd" cfg.reconcileCmd ]
    ++ lib.concatMap (p: [ "--peer" p ]) cfg.peers
    ++ lib.optional cfg.verbose "-v"
    ++ cfg.extraArgs;

  env = (lib.optionalAttrs (cfg.logLevel != null) { RUST_LOG = cfg.logLevel; }) // cfg.extraEnv;

  # `tailscale ip -4` is how the daemon discovers its listen address when
  # `--listen` is omitted (see src/main.rs), and `sh` is what src/reconcile.rs
  # spawns the reconcile hook with. The launchd agent below picks a shell up
  # from /usr/bin:/bin, but a systemd user unit inherits no PATH at all, so
  # without one here the hook dies with ENOENT before it ever runs.
  servicePath = lib.makeBinPath [
    pkgs.runtimeShellPackage
    cfg.tailscalePackage
  ];

  exe = lib.getExe cfg.package;

  peerHasPort = p: builtins.match ".+:[0-9]+" p != null;
in
{
  options.services.themed = {
    enable = lib.mkEnableOption "the themed theme-sync daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.themed;
      defaultText = lib.literalExpression "pkgs.themed";
      description = "The themed package to run.";
    };

    tailscalePackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.tailscale;
      defaultText = lib.literalExpression "pkgs.tailscale";
      description = ''
        Package providing the `tailscale` CLI, placed on the service PATH so the
        daemon can discover its listen address with `tailscale ip -4` when
        {option}`services.themed.listen` is null.
      '';
    };

    selfName = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Value for `--self`, the host name used only to break version ties.
        Null lets the daemon fall back to the system hostname.
      '';
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 47100;
      description = ''
        Peer port. Only used to build a default {option}`listen` address; it is
        not passed to the daemon on its own.
      '';
    };

    listen = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "100.64.0.1:47100";
      description = ''
        Value for `--listen` (`address:port`). Null omits the flag, so the
        daemon runs `tailscale ip -4` and binds `<that>:${toString cfg.port}`,
        retrying until tailscaled answers.
      '';
    };

    stateFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Value for `--state-file`. Null uses the daemon default
        (`$XDG_STATE_HOME/themed/state.json`, else
        `~/.local/state/themed/state.json`).
      '';
    };

    socket = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Value for `--socket`, the control socket path. Null uses the daemon
        default: `$XDG_RUNTIME_DIR/themed.sock`, or, where there is no
        `XDG_RUNTIME_DIR` (macOS), `$XDG_STATE_HOME/themed/themed.sock` —
        which the `themed set/get` CLI derives the same way.
      '';
    };

    reconcileCmd = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/nix/store/...-themed-reconcile";
      description = ''
        Value for `--reconcile-cmd`; run via `sh -c` with the theme blob on
        stdin on every change. Only a shell and {option}`tailscalePackage` are
        on the service PATH, so the command has to name whatever else it needs
        by absolute path.
      '';
    };

    peers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = lib.literalExpression ''
        [ "fezzik.example.ts.net:47100" "vizzini.example.ts.net:47100" ]
      '';
      description = "Peers to push to, each as `host:port`. One `--peer` per entry; an empty list is valid.";
    };

    verbose = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Pass `-v` for per-message tracing.";
    };

    logLevel = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "themed=debug";
      description = "Sets `RUST_LOG` in the service environment.";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra arguments appended verbatim to the daemon invocation.";
    };

    extraEnv = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Extra environment variables for the service.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = isLinux || isDarwin;
        message = "services.themed is only supported on Linux (systemd --user) and Darwin (launchd).";
      }
      {
        assertion = lib.all peerHasPort cfg.peers;
        message =
          "services.themed.peers entries must be `host:port`; these are missing a port: "
          + lib.concatStringsSep ", " (lib.filter (p: !peerHasPort p) cfg.peers);
      }
    ];

    systemd.user.services = lib.mkIf isLinux {
      themed = {
        Unit = {
          Description = "themed — replicated theme register";
          After = [
            "network-online.target"
            "tailscaled.service"
          ];
          Wants = [ "network-online.target" ];
        };
        Service = {
          ExecStart = "${exe} ${lib.escapeShellArgs args}";
          Environment =
            [ "PATH=${servicePath}" ]
            ++ lib.mapAttrsToList (n: v: "${n}=${v}") env;
          Restart = "on-failure";
          RestartSec = 2;
        };
        Install.WantedBy = [ "default.target" ];
      };
    };

    launchd.agents = lib.mkIf isDarwin {
      themed = {
        enable = true;
        config = {
          ProgramArguments = [ exe ] ++ args;
          KeepAlive = true;
          RunAtLoad = true;
          EnvironmentVariables = {
            PATH = "${servicePath}:/usr/bin:/bin";
          } // env;
          StandardOutPath = "${config.home.homeDirectory}/Library/Logs/themed.log";
          StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/themed.log";
        };
      };
    };
  };
}
