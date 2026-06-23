# Katsuobushi agent sandbox.
#
# Assembles a `microvm.nix` guest (a real NixOS system booted under qemu with a
# genuine kernel boundary) that comes up as a working local dev environment in
# which an agent harness — Claude Code by default — can run with its blast
# radius bounded by the VM rather than by permission prompts. See
# `design/sandbox.md` for the complete design and the rationale behind every
# decision referenced in comments below (e.g. "4.5" → decision 4.5).
#
# Like the other Katsuobushi libs this module is partial-applied by the flake
# with its pinned infra dependency (`{ microvm }`); the resulting function is
# what a consumer calls as `katsuobushi.lib.sandbox { inherit pkgs; ... }`.
# `microvm` is exposed as an optional argument so it stays overridable per-call.
#
# The whole VM is hermetic: the proxy, firewall, allowlist, DNS policy, and
# agent environment are baked into the guest image and enforced by guest init.
# The host runs only one `qemu` process per instance — there is no shared
# host-side daemon — which is what makes running many instances/projects trivial
# (3.). Per-instance dynamic values (the bare-mirror path, the ssh port, secret
# files) are emitted into the qemu invocation at launch via microvm.nix's
# `extraArgsScript`, so nothing instance-specific is baked into the store.
defaults:
{
  pkgs,
  # Path to the consumer's project (e.g. `./.`). Used at launch by the host
  # runner to build the per-instance bare mirror; not baked into the image.
  workspaceRoot,
  # Stable, owner-qualified identifier (e.g. "cdata/katsuobushi"). Names the
  # well-known in-guest project path and the per-instance host state dirs.
  projectId,

  # --- Network egress (4.5, 4.6) ---
  # Extra reachable origins, appended to `baseAllowedOrigins`. Hostnames only;
  # each becomes a squid `dstdomain`. No implicit wildcards — "github.com"
  # matches only that host; ".github.com" opts into the subtree.
  allowedOrigins ? [ ],
  # The lean baseline every sandbox gets. There is deliberately no per-entry
  # subtraction (4.6); to "remove" a baseline host, override this list wholesale.
  baseAllowedOrigins ? [
    # Anthropic inference. CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 (set in the
    # guest env) keeps Claude Code's telemetry/feature-flag/autoupdate hosts off
    # the allowlist — the single biggest lever for a small baseline.
    "api.anthropic.com"
    # OAuth/subscription auth validation. Claude Code contacts this even with
    # nonessential traffic disabled; without it auth fails with ERR_BAD_REQUEST.
    "platform.claude.com"
    # Nix: substituters + the GitHub flake-input hosts. The minimum for in-guest
    # `nix develop` plus typical github flake inputs. Trim per-host if your flake
    # has no github inputs (see design 4.6 — pin this set empirically).
    "cache.nixos.org"
    "channels.nixos.org"
    "github.com"
    "api.github.com"
    "codeload.github.com"
  ],

  # --- Reference repos (4.12): build-time pinned, writable copies ---
  # List of { source, dest }. `source` is a store path / derivation (a flake
  # input with `flake = false`, or a `pkgs.fetchFromGitHub { ... }`). `dest` is
  # relative to the agent home; mirror the host Git-layout convention, e.g.
  # "Git/github.com/oxalica/rust-overlay".
  extraRepos ? [ ],

  # --- Untracked workspace context (4.10): project-scoped, one-way host->guest.
  # List of project-relative paths (e.g. ".claude", "notes") carried into the
  # workspace on top of the mirror clone. Absolute paths and ".." are rejected
  # at eval time; symlink escape is refused at copy time by the host runner.
  workspaceContext ? [ ],

  # --- Home-file mappings (4.14): dest (in agent home) -> { source, path?, mode }
  # `source` is a store path; `path` an optional subpath within it. Modes:
  #   "immutable" — read-only bind mount; fixed even against the agent.
  #   "seed"      — copied into home at boot; the agent may edit it.
  #   "link"      — store symlink; present but replaceable (cheapest).
  homeFiles ? { },

  # --- Runtime secrets (4.17): NAME -> { fromEnv = "VAR"; } | { fromFile = "P"; }
  # The declaration lives here; the value is read from the host by the runner at
  # launch and injected via fw_cfg (never in the store, argv, or on disk). The
  # runner fails fast if a declared secret is missing.
  secrets ? { },

  # --- Resources ---
  vcpu ? 4,
  mem ? 8192, # MiB. NB: qemu hangs at exactly 2048 (microvm.nix#171).
  storeOverlaySize ? "8G", # tmpfs writable /nix/store overlay; heavy builds need more.

  # The Claude Code package to install in the guest. Defaults to nixpkgs'
  # `claude-code` if present (it is unfree — the consumer's `pkgs` must allow
  # it), else null, in which case the manifest still describes the workflow and
  # the consumer can supply a package. Resolved from the *consumer's* pkgs so an
  # `allowUnfree` config carries through.
  claudeCodePackage ? (pkgs.claude-code or null),

  # Escape hatch: extra NixOS modules merged into the guest (4 / §5).
  guestModules ? [ ],

  # Infra dependency, defaulting to the version Katsuobushi pins.
  microvm ? defaults.microvm,
}:

let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;

  # Bare project name (drops the owner qualifier), used for the well-known path.
  projectName = baseNameOf projectId;

  # Effective, fully-resolved egress allowlist. The manifest always prints this
  # so the agent and the human see exactly what is reachable (4.6 transparency).
  effectiveAllowedOrigins = baseAllowedOrigins ++ allowedOrigins;

  # In-guest constants.
  agentUser = "agent";
  agentHome = "/home/${agentUser}";
  workspaceParent = "/workspace";
  workspacePath = "${workspaceParent}/${projectName}";
  proxyPort = 3128;
  proxyUid = 3128; # fixed so the nftables uid match is deterministic.
  guestMac = "02:00:00:00:00:02";
  # The whole per-instance host state dir is exposed to the guest as one 9p
  # share (4.8 — exactly one host directory of write access). It holds sync.git,
  # context/, status.json, console.log, and the small non-secret runtime files
  # (instance name, mode, task, authorized_keys). Secrets never go here.
  shareTag = "katsuobushi";
  shareMount = "/mnt/katsuobushi";
  # slirp user-mode networking puts the built-in DNS forwarder here. squid (and
  # only squid) is allowed to use it; the agent gets no resolver at all (4.5).
  slirpDns = "10.0.2.3";

  secretNames = builtins.attrNames secrets;

  # ---- Eval-time validation of project-scoped paths (4.10 enforcement) -------
  checkContextPath =
    p:
    if lib.hasPrefix "/" p then
      throw ''
        katsuobushi.lib.sandbox: workspaceContext entry "${p}" must be a path
        relative to the project root, not an absolute path.''
    else if
      (p == "..") || (lib.hasPrefix "../" p) || (lib.hasInfix "/../" p) || (lib.hasSuffix "/.." p)
    then
      throw ''
        katsuobushi.lib.sandbox: workspaceContext entry "${p}" must not escape
        the project root with "..".''
    else
      p;
  validatedContext = map checkContextPath workspaceContext;

  checkRepoDest =
    r:
    if lib.hasPrefix "/" r.dest then
      throw ''katsuobushi.lib.sandbox: extraRepos dest "${r.dest}" must be relative to the agent home.''
    else if (r.dest == "..") || (lib.hasPrefix "../" r.dest) || (lib.hasInfix "/../" r.dest) then
      throw ''katsuobushi.lib.sandbox: extraRepos dest "${r.dest}" must not contain "..".''
    else
      r;
  validatedRepos = map checkRepoDest extraRepos;

  # ---- Discoverability manifest (4.15) --------------------------------------
  originBullets = lib.concatMapStrings (o: "- `${o}` (HTTPS)\n") effectiveAllowedOrigins;
  repoBullets =
    if validatedRepos == [ ] then
      "_None declared._\n"
    else
      lib.concatMapStrings (r: "- `${agentHome}/${r.dest}`\n") validatedRepos;

  manifest = pkgs.writeText "katsuobushi-README.md" ''
    # Katsuobushi sandbox

    You are running inside an ephemeral Katsuobushi sandbox VM. This file
    describes the shape of the environment so you do not have to discover it by
    trial and error.

    ## Your project

    A working clone of **${projectId}** lives at:

    ```
    ${workspacePath}
    ```

    It started from a snapshot of the human's working tree (tracked + staged
    files), not just the last commit, so you begin from what they actually see
    on disk.

    **Returning work to the human is ordinary git.** Commit on the branch
    `sandbox/<instance>` (already checked out) and `git push`. The branch then
    appears back on the host — that *is* the delivery mechanism. Nothing else in
    this VM survives teardown.

    ## Reference repositories

    Read-only-provenance, writable copies of additional repos (branch/build/
    experiment freely):

    ${repoBullets}
    ## The network is not the open internet

    There is **no general internet access**. DNS is disabled by design — name
    resolution will simply fail. Outbound traffic is default-deny; the only way
    out is an HTTPS proxy (already set in `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`)
    which permits **only** these origins:

    ${originBullets}
    Do not waste turns trying to reach anything else or fighting the firewall —
    it is enforced below your privilege level and cannot be changed from here.

    ## How to work

    This is a Nix flake project. The sanctioned entry workflow is:

    ```
    nix develop
    ```

    which drops you into the project's dev shell (run `showMenu` there to see the
    project's build/test/format commands). You may extend the flake exactly as on
    the host; in-guest `nix` builds work against a writable store overlay.

    ## Reporting status

    For long/autonomous runs, write progress to `${shareMount}/status.json`, e.g.
    `{"phase":"working","ready":false}`. A pushed `sandbox/<instance>` branch is
    itself the strong "ready" signal.

    ## Things you are *not* able to do here

    - Reach arbitrary network hosts (only the allowlist above, via the proxy).
    - Resolve DNS (there is no resolver).
    - Touch the host system or other projects (a real kernel boundary separates
      you).
    - Persist anything beyond the branch you push and files you write into
      `${shareMount}`.
    - Use the human's git credentials or upstream remotes — they are not present.
  '';

  # homeFiles always includes the generated manifest as an internal immutable
  # entry at ~/README.md, plus a guest ~/.claude/CLAUDE.md that imports it so
  # every in-VM agent session auto-loads it (4.15). The project's own
  # CLAUDE.md/AGENTS.md still layer normally on top inside the workspace.
  claudeImport = pkgs.writeText "katsuobushi-CLAUDE.md" ''
    # Sandbox environment

    This machine is a Katsuobushi sandbox. Read `@${agentHome}/README.md` for the
    full description of what is available here, how to return work, and what the
    network allows.
  '';

  # Pre-seed Claude Code's per-user state so a brand-new ephemeral home does not
  # trap an interactive session behind the first-run gates. Empirically (the
  # docs do not publish this schema) a fresh `claude` TUI otherwise stops on:
  #   1. the onboarding/theme wizard ("Welcome to Claude Code… choose a theme"),
  #   2. the per-folder "do you trust this folder?" dialog,
  # and never reaches the prompt — so it appears to "ignore" CLAUDE_CODE_OAUTH_TOKEN
  # even though `claude -p` (which skips these gates) authenticates fine.
  # Seeding `hasCompletedOnboarding` + a theme + pre-trusting the workspace and
  # home paths takes the session straight to an authenticated prompt. This is a
  # seed (writable) file: Claude rewrites ~/.claude.json at runtime.
  claudeConfigSeed = pkgs.writeText "claude.json" (
    builtins.toJSON {
      hasCompletedOnboarding = true;
      theme = "dark";
      projects = {
        ${workspacePath} = {
          hasTrustDialogAccepted = true;
          hasCompletedProjectOnboarding = true;
        };
        ${agentHome} = {
          hasTrustDialogAccepted = true;
          hasCompletedProjectOnboarding = true;
        };
      };
    }
  );

  # NB: we deliberately do NOT seed `permissions.defaultMode = "bypassPermissions"`
  # into ~/.claude/settings.json — that triggers Claude Code's own startup
  # acknowledgement gate, re-blocking the interactive session. Autonomous runs
  # auto-approve via the `--dangerously-skip-permissions` flag instead; an
  # interactive user can opt in the same way.

  # Layering: lib defaults (overridable by the consumer's homeFiles) < consumer
  # homeFiles < the lib's own immutable manifest files (always ours).
  defaultHomeFiles = {
    ".claude.json" = {
      source = claudeConfigSeed;
      mode = "seed";
    };
  };

  effectiveHomeFiles = defaultHomeFiles // homeFiles // {
    "README.md" = {
      source = manifest;
      mode = "immutable";
    };
    ".claude/CLAUDE.md" = {
      source = claudeImport;
      mode = "immutable";
    };
  };

  # Resolve a homeFiles entry to the concrete source file path in the store.
  homeFileSource =
    entry:
    if entry ? path && entry.path != null then "${entry.source}/${entry.path}" else "${entry.source}";

  homeFilesList = lib.mapAttrsToList (dest: entry: {
    inherit dest;
    src = homeFileSource entry;
    mode = entry.mode;
  }) effectiveHomeFiles;

  immutableHomeFiles = builtins.filter (e: e.mode == "immutable") homeFilesList;
  seedHomeFiles = builtins.filter (e: e.mode == "seed") homeFilesList;
  linkHomeFiles = builtins.filter (e: e.mode == "link") homeFilesList;

  # ---- squid forward-proxy configuration (4.5) ------------------------------
  # `dstdomain` allowlist with `http_access deny all` as the backstop; squid
  # resolves names itself via the slirp DNS forwarder (the agent has none).
  squidConf = pkgs.writeText "squid.conf" ''
    http_port 127.0.0.1:${toString proxyPort}

    # Without DNS, squid cannot resolve its own hostname and FATALs at startup
    # ("Could not determine fully qualified hostname"). Pin it explicitly.
    visible_hostname katsuobushi

    # Resolve via slirp's built-in forwarder explicitly, so squid works even
    # though /etc/resolv.conf is a no-op for the unprivileged agent.
    dns_nameservers ${slirpDns}

    acl SSL_ports port 443
    acl Safe_ports port 80
    acl Safe_ports port 443
    acl CONNECT method CONNECT

    # The generated hostname allowlist (effective = base ++ consumer).
    ${lib.concatMapStrings (o: "acl allowed dstdomain ${o}\n") effectiveAllowedOrigins}
    http_access deny CONNECT !SSL_ports
    http_access deny !Safe_ports
    http_access allow allowed
    http_access deny all

    # Memory-only; no on-disk cache to provision.
    cache deny all
    coredump_dir /run/katsuproxy
    pid_filename /run/katsuproxy/squid.pid
    # Log into the (RAM-backed, ephemeral) runtime dir. squid's logfile module
    # fopen()s these paths, so /dev/stdout|/dev/stderr — which are sockets under
    # systemd — make it FATAL at startup; real files avoid that.
    cache_log /run/katsuproxy/cache.log
    access_log stdio:/run/katsuproxy/access.log
    shutdown_lifetime 1 seconds
  '';

  # ---- Host runner: launch-time argument emission (4.17 channel A, §7) -------
  # microvm.nix runs this at launch and splices its single line of stdout into
  # the qemu invocation. It reads only env/paths prepared by the wrapper, so
  # nothing instance-specific is in the store. (verifies design checklist #2)
  extraArgsScript = pkgs.writeShellScript "katsuobushi-extra-args" ''
    set -eu
    args=""

    # User-mode (slirp) NIC with an ssh hostfwd bound to host loopback only
    # (4.18). passt is unsupported by microvm.nix's qemu runner, so we use the
    # guaranteed slirp fallback (4.4).
    args="$args -netdev user,id=net0,hostfwd=tcp:127.0.0.1:''${KATSU_SSH_PORT}-:22"
    args="$args -device virtio-net-pci,netdev=net0,mac=${guestMac},romfile="

    # Per-instance state dir as one rw 9p share (4.8).
    args="$args -fsdev local,id=katsushare,path=''${KATSU_STATE_DIR},security_model=none"
    args="$args -device virtio-9p-pci,fsdev=katsushare,mount_tag=${shareTag}"

    # Declared secrets via fw_cfg, reading from tmpfs files the wrapper staged.
    ${lib.concatMapStrings (name: ''
      args="$args -fw_cfg name=opt/io.systemd.credentials/${name},file=''${KATSU_CRED_${name}}"
    '') secretNames}

    printf '%s' "$args"
  '';

  # ---- The guest NixOS system -----------------------------------------------
  guestModule =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    {
      # Boot/runner shape.
      microvm = {
        hypervisor = "qemu";
        inherit vcpu mem;
        # No static interfaces: the NIC (with its per-instance hostfwd port) is
        # emitted by extraArgsScript so parallel instances do not collide.
        interfaces = [ ];
        extraArgsScript = "${extraArgsScript}";
        # Shared host /nix/store (ro) + a writable overlay so in-guest `nix
        # develop` builds work (4.13).
        shares = [
          {
            tag = "ro-store";
            source = "/nix/store";
            mountPoint = "/nix/.ro-store";
            proto = "9p";
          }
        ];
        writableStoreOverlay = "/nix/.rw-store";
      };

      # RAM-backed writable store overlay, bounded by storeOverlaySize (4.13 /
      # open risk: heavy builds can exhaust it — documented knob).
      fileSystems."/nix/.rw-store" = {
        device = "rwstore";
        fsType = "tmpfs";
        options = [
          "size=${storeOverlaySize}"
          "mode=0755"
        ];
        neededForBoot = true;
      };

      # The per-instance 9p share (emitted by extraArgsScript above). nofail so
      # building the image as a check, or any boot without the share, still
      # comes up.
      fileSystems.${shareMount} = {
        device = shareTag;
        fsType = "9p";
        options = [
          "trans=virtio"
          "version=9p2000.L"
          "msize=131072"
          "nofail"
          "x-systemd.after=systemd-modules-load.service"
        ];
      };

      networking.hostName = "katsuobushi";
      # Ephemeral guest; pin a stateVersion so the eval is reproducible.
      system.stateVersion = "25.11";
      # systemd-networkd (microvm.optimize default) — DHCP on the slirp NIC,
      # matched by MAC. UseDNS=false so the agent gets no resolver via DHCP.
      systemd.network.networks."10-katsu" = {
        matchConfig.MACAddress = guestMac;
        networkConfig.DHCP = "ipv4";
        dhcpV4Config.UseDNS = false;
      };

      # ---- Users (6.1) ----
      users.mutableUsers = false;
      # Intentional: there is no root/password login. The agent authenticates
      # with the ephemeral key injected at launch (4.18); root is unreachable.
      users.allowNoPasswordLogin = true;
      users.users.${agentUser} = {
        isNormalUser = true;
        home = agentHome;
        # No sudo / wheel: the agent runs strictly unprivileged so the in-guest
        # firewall is a genuine boundary against it (4.3).
        extraGroups = [ ];
      };
      users.users.katsuproxy = {
        isSystemUser = true;
        group = "katsuproxy";
        uid = proxyUid;
      };
      users.groups.katsuproxy.gid = proxyUid;

      # ---- Firewall: nftables default-deny egress (6.3) ----
      # The agent's only path out is squid on loopback; only the squid user may
      # talk to the network. DHCP is allowed so the NIC can get an address.
      networking.nftables.enable = true;
      networking.nftables.ruleset = ''
        table inet katsuobushi {
          chain input {
            type filter hook input priority 0; policy drop;
            ct state established,related accept
            iif "lo" accept
            tcp dport 22 accept
          }
          chain forward {
            type filter hook forward priority 0; policy drop;
          }
          chain output {
            type filter hook output priority 0; policy drop;
            ct state established,related accept
            oif "lo" accept
            # DHCP client (get an IP from slirp)
            udp dport { 67, 68 } accept
            # Only squid reaches the network: DNS to the slirp forwarder and
            # outbound HTTP/HTTPS to the resolved allowlist targets.
            meta skuid ${toString proxyUid} accept
            # Everything else (agent raw sockets, port 53, arbitrary TCP/UDP)
            # is dropped.
            counter drop
          }
        }
      '';
      # The stock NixOS firewall is redundant with our explicit ruleset.
      networking.firewall.enable = false;

      # ---- squid proxy (6.4) ----
      systemd.services.katsuproxy = {
        description = "Katsuobushi egress allowlist proxy (squid)";
        wantedBy = [ "multi-user.target" ];
        # squid binds loopback and connects out lazily, so it needs the network
        # stack but must not block on network-online.target (microvm.optimize
        # disables wait-online, so that target may never settle).
        after = [ "network.target" ];
        serviceConfig = {
          User = "katsuproxy";
          Group = "katsuproxy";
          RuntimeDirectory = "katsuproxy";
          ExecStart = "${pkgs.squid}/bin/squid -N -f ${squidConf} -d1";
          Restart = "on-failure";
          # Surface squid startup failures on the teed serial console.
          StandardError = "journal+console";
        };
      };

      # ---- Agent environment (6.10) ----
      # System-wide proxy + Claude Code hygiene. The OAuth token is *not* here;
      # it is delivered as a runtime secret (see katsuobushi-credentials below).
      environment.variables = {
        HTTPS_PROXY = "http://127.0.0.1:${toString proxyPort}";
        HTTP_PROXY = "http://127.0.0.1:${toString proxyPort}";
        ALL_PROXY = "http://127.0.0.1:${toString proxyPort}";
        https_proxy = "http://127.0.0.1:${toString proxyPort}";
        http_proxy = "http://127.0.0.1:${toString proxyPort}";
        all_proxy = "http://127.0.0.1:${toString proxyPort}";
        # No connection to api.anthropic.com beyond inference: keeps Claude
        # Code's ancillary hosts off the allowlist (4.6).
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "1";
      };

      # nix-daemon downloads via the proxy too, so substituters are reachable
      # only through the allowlist (6.5). Loopback to squid is permitted.
      systemd.services.nix-daemon.environment = {
        https_proxy = "http://127.0.0.1:${toString proxyPort}";
        http_proxy = "http://127.0.0.1:${toString proxyPort}";
      };
      nix.settings = {
        experimental-features = [
          "nix-command"
          "flakes"
        ];
        substituters = [ "https://cache.nixos.org" ];
        trusted-users = [ agentUser ];
      };

      # ---- ssh: key-only, agent only, reachable only via the loopback hostfwd
      # (4.18). The pubkey arrives per-launch through the share.
      services.openssh = {
        enable = true;
        settings = {
          PasswordAuthentication = false;
          KbdInteractiveAuthentication = false;
          PermitRootLogin = "no";
          AllowUsers = [ agentUser ];
        };
      };

      # ---- Login greeting + per-session secret export + env hygiene (4.18) ----
      # Added to /etc/profile (NixOS does NOT source /etc/profile.d/*.sh, so this
      # is the hook that actually runs for ssh logins and the autonomous
      # `bash -lc` launcher). Exports each delivered secret as its env var,
      # unsets any stray Anthropic key that would outrank the OAuth token (4.17
      # precedence hygiene), and prints the manifest on an interactive terminal.
      environment.loginShellInit = ''
        # Anthropic env hygiene: only the OAuth token should authenticate.
        unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN
        if [ -d /run/katsuobushi/secrets ]; then
          for _s in /run/katsuobushi/secrets/*; do
            [ -e "$_s" ] || continue
            # Strip CR/LF: a token pasted from wrapped `claude setup-token`
            # output carries embedded newlines, which make the Authorization
            # header value illegal ("Header has invalid value"). Tokens never
            # contain whitespace, so this is safe.
            export "$(basename "$_s")"="$(tr -d '\r\n' < "$_s")"
          done
          unset _s
        fi
        if [ -r ${agentHome}/README.md ] && [ -t 1 ]; then
          cat ${agentHome}/README.md
        fi
        # Land in the (pre-trusted) workspace so `claude` starts in the project.
        if [ -d ${workspacePath} ]; then cd ${workspacePath}; fi
      '';

      environment.systemPackages =
        with pkgs;
        [
          git
          coreutils
          gnutar
          gzip
          rsync
          cacert
        ]
        ++ lib.optional (claudeCodePackage != null) claudeCodePackage;

      # CA bundle so HTTPS-through-proxy validates.
      security.pki.certificateFiles = [ "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];

      # /workspace owned by the agent (4 / §6.8).
      systemd.tmpfiles.rules = [
        "d ${workspaceParent} 0755 ${agentUser} users - -"
        "d /run/katsuobushi 0755 root root - -"
      ]
      # seed homeFiles: copy from store into home, agent may edit (4.14).
      ++ map (e: "C ${agentHome}/${e.dest} 0644 ${agentUser} users - ${e.src}") seedHomeFiles
      # link homeFiles: store symlink, replaceable (4.14).
      ++ map (e: "L+ ${agentHome}/${e.dest} - - - - ${e.src}") linkHomeFiles;

      # ---- Secret delivery (4.17) ----
      # Pulls each fw_cfg system credential and writes it to a tmpfs file
      # readable only by the agent, so both the interactive login shell and the
      # autonomous launcher can export it. /run is RAM; nothing hits disk.
      systemd.services.katsuobushi-credentials = lib.mkIf (secretNames != [ ]) {
        description = "Stage Katsuobushi runtime secrets for the agent";
        wantedBy = [ "multi-user.target" ];
        before = [ "katsuobushi-agent.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ImportCredential = secretNames;
          # A missing credential is surfaced on the teed serial console.
          StandardError = "journal+console";
        };
        script = ''
          install -d -m 0750 -o ${agentUser} -g users /run/katsuobushi/secrets
          ${lib.concatMapStrings (name: ''
            if [ -r "$CREDENTIALS_DIRECTORY/${name}" ]; then
              install -m 0400 -o ${agentUser} -g users \
                "$CREDENTIALS_DIRECTORY/${name}" /run/katsuobushi/secrets/${name}
            else
              echo "katsuobushi: secret ${name} was not delivered to the guest" >&2
            fi
          '') secretNames}
        '';
      };

      # ---- Inject the per-launch ssh pubkey (4.18) ----
      # The wrapper drops the ephemeral pubkey into the share; install it into
      # the agent's authorized_keys before sshd accepts connections.
      systemd.services.katsuobushi-authkeys = {
        description = "Install the ephemeral agent ssh authorized key";
        wantedBy = [ "multi-user.target" ];
        before = [ "sshd.service" ];
        after = [ "${lib.replaceStrings [ "/" ] [ "-" ] (lib.removePrefix "/" shareMount)}.mount" ];
        unitConfig.ConditionPathExists = "${shareMount}/authorized_keys";
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          install -d -m 0700 -o ${agentUser} -g users ${agentHome}/.ssh
          install -m 0600 -o ${agentUser} -g users \
            ${shareMount}/authorized_keys ${agentHome}/.ssh/authorized_keys
        '';
      };

      # ---- Immutable homeFiles: per-file read-only bind mounts (4.14) ----
      # A symlink would be removable by the agent (it owns its home); a per-file
      # RO bind mount cannot be overwritten or unmounted unprivileged. Done in a
      # root service (rather than `fileSystems`) so single-file mountpoints over
      # the tmpfs home are created reliably after the home exists.
      systemd.services.katsuobushi-homefiles = {
        description = "Install immutable Katsuobushi home files";
        wantedBy = [ "multi-user.target" ];
        after = [ "local-fs.target" ];
        before = [ "sshd.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = lib.concatMapStrings (e: ''
          install -d -m 0755 -o ${agentUser} -g users "$(dirname ${agentHome}/${e.dest})"
          if ! mountpoint -q ${agentHome}/${e.dest} 2>/dev/null; then
            : > ${agentHome}/${e.dest} || true
            chown ${agentUser}:users ${agentHome}/${e.dest} || true
            ${pkgs.util-linux}/bin/mount --bind ${e.src} ${agentHome}/${e.dest}
            ${pkgs.util-linux}/bin/mount -o remount,bind,ro ${agentHome}/${e.dest}
          fi
        '') immutableHomeFiles;
      };

      # ---- Workspace materialization (4.11) ----
      # Clone the bare mirror (its only remote is the sync point — no host
      # credentials/upstreams leak), check out the seed branch, overlay the
      # untracked context, and lay down the writable reference-repo copies.
      systemd.services.katsuobushi-workspace = {
        description = "Materialize the Katsuobushi workspace";
        wantedBy = [ "multi-user.target" ];
        after = [
          "${lib.replaceStrings [ "/" ] [ "-" ] (lib.removePrefix "/" shareMount)}.mount"
          "katsuobushi-homefiles.service"
        ];
        path = with pkgs; [
          git
          coreutils
          rsync
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          User = agentUser;
          Group = "users";
          # Surface failures/trace on the teed serial console for debuggability.
          StandardOutput = "journal+console";
          StandardError = "journal+console";
        };
        script = ''
          set -eu
          export HOME=${agentHome}
          if [ ! -e ${shareMount}/sync.git ]; then
            echo "no sync share present; skipping workspace setup"
            exit 0
          fi
          instance="$(cat ${shareMount}/instance 2>/dev/null || echo unknown)"

          # The bare mirror comes over a 9p share owned by the host user; git
          # refuses to operate on a repo it considers "dubiously owned" unless we
          # mark these trees safe. Covers both the share and the clone.
          git config --global --add safe.directory '*'
          git config --global user.email "agent@katsuobushi.local"
          git config --global user.name "Katsuobushi agent"

          if [ ! -d ${workspacePath}/.git ]; then
            git clone ${shareMount}/sync.git ${workspacePath}
          fi
          cd ${workspacePath}
          git checkout "sandbox/$instance" 2>/dev/null || git checkout -b "sandbox/$instance"

          # Overlay declared untracked context (.git excluded so the clean
          # linkage wins). Host-side staging already refused symlink escapes.
          if [ -d ${shareMount}/context ]; then
            rsync -a --exclude='.git' ${shareMount}/context/ ${workspacePath}/
          fi

          # Writable reference-repo copies (4.12).
          ${lib.concatMapStrings (r: ''
            mkdir -p "$(dirname ${agentHome}/${r.dest})"
            if [ ! -e ${agentHome}/${r.dest} ]; then
              cp -rT ${r.source} ${agentHome}/${r.dest}
              chmod -R u+w ${agentHome}/${r.dest}
            fi
          '') validatedRepos}
        '';
      };

      # ---- Autonomous run mode (4.18) ----
      # Always present; no-ops in interactive mode. In autonomous mode it runs
      # `claude -p "<task>"` as the agent (loading the proxy/secret profile),
      # writes status phases, pushes the branch, and powers off unless asked to
      # linger. Runs as root only to orchestrate su/poweroff; claude runs as the
      # unprivileged agent.
      systemd.services.katsuobushi-agent = {
        description = "Katsuobushi autonomous agent run";
        wantedBy = [ "multi-user.target" ];
        after = [
          "katsuobushi-workspace.service"
          "katsuproxy.service"
          "network-online.target"
        ];
        wants = [ "network-online.target" ];
        path = with pkgs; [
          git
          coreutils
          util-linux
          systemd
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          StandardOutput = "journal+console";
          StandardError = "journal+console";
        };
        script = ''
          set -u
          mode="$(cat ${shareMount}/mode 2>/dev/null || echo interactive)"
          if [ "$mode" != "autonomous" ]; then
            exit 0
          fi
          instance="$(cat ${shareMount}/instance 2>/dev/null || echo unknown)"
          status() { echo "{\"phase\":\"$1\",\"ready\":$2}" > ${shareMount}/status.json 2>/dev/null || true; }

          status running false
          task="$(cat ${shareMount}/task 2>/dev/null || true)"
          # Run claude as the agent with a login shell so the proxy/secret
          # profile is applied. Not --bare (it ignores CLAUDE_CODE_OAUTH_TOKEN).
          # --dangerously-skip-permissions auto-approves tool calls — headless
          # -p cannot answer prompts, and the VM is the blast-radius boundary
          # (the whole point of the sandbox). bash is given by absolute path:
          # runuser execs via the service PATH, which does not contain bash.
          runuser -u ${agentUser} -- ${pkgs.bash}/bin/bash -lc "cd ${workspacePath} && claude --dangerously-skip-permissions -p \"$task\"" \
            && rc=0 || rc=$?

          # Push whatever the agent committed back to the host (4.8 push-back).
          runuser -u ${agentUser} -- ${pkgs.bash}/bin/bash -lc "cd ${workspacePath} && git push origin HEAD:sandbox/$instance" || true

          if [ "$rc" -eq 0 ]; then status ready true; else status failed false; fi

          if [ "$(cat ${shareMount}/keepalive 2>/dev/null || echo no)" != "yes" ]; then
            systemctl poweroff
          fi
        '';
      };
    };

  guestSystem = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    inherit system;
    modules = [
      microvm.nixosModules.microvm
      guestModule
    ]
    ++ guestModules;
  };

  runner = guestSystem.config.microvm.declaredRunner;

  # ---- Host-side wrapper (the `nix run .#sandbox` app, §7) -------------------
  # Resolves the instance, builds the per-instance bare mirror + working-tree
  # snapshot seed, stages context (symlink-safe) + non-secret runtime files,
  # reads each secret from the host into a tmpfs file (fail-fast), generates an
  # ephemeral ssh keypair, boots, and either attaches over ssh (interactive) or
  # waits for the autonomous run to finish. The pushed branch, status.json, and
  # console.log persist in the host state dir; the rest evaporates.
  sandboxRunner = pkgs.writeShellApplication {
    name = "sandbox";
    runtimeInputs = with pkgs; [
      git
      openssh
      coreutils
      rsync
      gnused
    ];
    text = ''
      mode="interactive"
      task=""
      keepalive="no"
      instance=""

      while [ "$#" -gt 0 ]; do
        case "$1" in
          --task) mode="autonomous"; task="$2"; shift 2 ;;
          --task-file) mode="autonomous"; task="$(cat "$2")"; shift 2 ;;
          --keep-alive) keepalive="yes"; shift ;;
          --name) instance="$2"; shift 2 ;;
          *) echo "sandbox: unknown argument: $1" >&2; exit 2 ;;
        esac
      done

      if [ -z "$instance" ]; then
        instance="$(date +%Y%m%d-%H%M%S)-$$"
      fi

      project="$(git rev-parse --show-toplevel)"
      state_root="''${XDG_STATE_HOME:-$HOME/.local/state}/katsuobushi/${projectId}/$instance"
      runtime_root="''${XDG_RUNTIME_DIR:-/tmp}/katsuobushi/${projectId}/$instance"
      mkdir -p "$state_root" "$runtime_root"
      chmod 700 "$runtime_root"

      echo '{"phase":"booting","ready":false}' > "$state_root/status.json"
      printf '%s' "$instance" > "$state_root/instance"
      printf '%s' "$mode"     > "$state_root/mode"
      printf '%s' "$keepalive" > "$state_root/keepalive"
      [ -n "$task" ] && printf '%s' "$task" > "$state_root/task"

      # Per-instance bare mirror + working-tree snapshot seed (4.9). `git stash
      # create` captures tracked+staged dirty state; fall back to HEAD when
      # clean. Pushing the snapshot ref transfers its objects into the mirror.
      if [ ! -d "$state_root/sync.git" ]; then
        git clone --bare "$project" "$state_root/sync.git" >/dev/null 2>&1
      fi
      snap="$(git -C "$project" stash create 2>/dev/null || true)"
      [ -z "$snap" ] && snap="$(git -C "$project" rev-parse HEAD)"
      git -C "$project" push --quiet "$state_root/sync.git" "$snap:refs/heads/sandbox/$instance" --force

      # Stage declared untracked context, refusing symlinks that escape the tree
      # (4.10 — rsync --safe-links drops out-of-tree link targets).
      rm -rf "$state_root/context"
      mkdir -p "$state_root/context"
      ${lib.concatMapStrings (p: ''
        if [ -e "$project/${p}" ]; then
          mkdir -p "$(dirname "$state_root/context/${p}")"
          rsync -a --safe-links "$project/${p}" "$(dirname "$state_root/context/${p}")/"
        fi
      '') validatedContext}

      # Read each declared secret from the host into a tmpfs file (fail-fast).
      ${lib.concatStrings (
        lib.mapAttrsToList (
          name: spec:
          if spec ? fromEnv then
            ''
              if [ -z "''${${spec.fromEnv}:-}" ]; then
                echo "sandbox: required secret ${name} is not set on the host." >&2
                echo "  Expected it in environment variable ${spec.fromEnv}." >&2
                echo "  e.g. export ${spec.fromEnv}=...  (use 'claude setup-token' for the OAuth token)" >&2
                exit 1
              fi
              printf '%s' "''${${spec.fromEnv}}" > "$runtime_root/cred-${name}"
              chmod 0600 "$runtime_root/cred-${name}"
              export KATSU_CRED_${name}="$runtime_root/cred-${name}"
            ''
          else if spec ? fromFile then
            ''
              if [ ! -r "${spec.fromFile}" ]; then
                echo "sandbox: required secret ${name} not readable at ${spec.fromFile}." >&2
                exit 1
              fi
              install -m 0600 "${spec.fromFile}" "$runtime_root/cred-${name}"
              export KATSU_CRED_${name}="$runtime_root/cred-${name}"
            ''
          else
            throw "katsuobushi.lib.sandbox: secret ${name} needs fromEnv or fromFile."
        ) secrets
      )}

      # Ephemeral ssh keypair (4.18); pubkey travels in the share, private key
      # stays in the runtime tmpfs.
      if [ ! -f "$runtime_root/id" ]; then
        ssh-keygen -t ed25519 -N "" -f "$runtime_root/id" -q
      fi
      # Installed into the agent's authorized_keys by katsuobushi-authkeys.
      cp "$runtime_root/id.pub" "$state_root/authorized_keys"

      # Pick a free loopback port for ssh and export it for extraArgsScript.
      pick_port() {
        local p
        for _ in $(seq 1 50); do
          p=$(( (RANDOM % 20000) + 20000 ))
          if ! (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then echo "$p"; return; fi
        done
        echo 22222
      }
      port="$(pick_port)"
      printf '%s' "$port" > "$state_root/ssh-port"

      export KATSU_STATE_DIR="$state_root"
      export KATSU_SSH_PORT="$port"

      cd "$runtime_root"

      if [ "$mode" = "autonomous" ]; then
        echo "sandbox: launching autonomous instance '$instance' (logs: $state_root/console.log)"
        ${runner}/bin/microvm-run 2>&1 | tee "$state_root/console.log"
        echo "sandbox: instance '$instance' finished. Fetch with: git fetch \"$state_root/sync.git\" \"sandbox/$instance\""
      else
        echo "sandbox: launching interactive instance '$instance' on 127.0.0.1:$port"
        ${runner}/bin/microvm-run > "$state_root/console.log" 2>&1 &
        vm=$!
        # Wait for sshd.
        for _ in $(seq 1 120); do
          if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then break; fi
          sleep 1
        done
        ssh -i "$runtime_root/id" -p "$port" \
          -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
          -o LogLevel=ERROR \
          ${agentUser}@127.0.0.1 || true
        ${runner}/bin/microvm-shutdown 2>/dev/null || kill "$vm" 2>/dev/null || true
        wait "$vm" 2>/dev/null || true
      fi
    '';
  };

  # ---- Lifecycle menu commands (§5) -----------------------------------------
  # Small helpers over the per-instance state dirs, ready to merge into makeMenu.
  stateGlob = "\${XDG_STATE_HOME:-$HOME/.local/state}/katsuobushi/${projectId}";
  menuCommands = {
    "sandbox:start" = {
      description = "Launch an agent sandbox VM (nix run .#sandbox)";
      command = "${sandboxRunner}/bin/sandbox \"$@\"";
    };
    "sandbox:list" = {
      description = "List sandbox instances for this project";
      command = ''
        root="${stateGlob}"
        [ -d "$root" ] || { echo "no instances"; exit 0; }
        for d in "$root"/*/; do
          [ -d "$d" ] || continue
          printf '%s\t%s\n' "$(basename "$d")" "$(cat "$d/status.json" 2>/dev/null || echo '{}')"
        done
      '';
    };
    "sandbox:status" = {
      description = "Show status.json for an instance: sandbox-status <instance>";
      command = ''cat "${stateGlob}/$1/status.json"'';
    };
    "sandbox:fetch" = {
      description = "Fetch an instance's branch into this repo: sandbox-fetch <instance>";
      command = ''
        git fetch "${stateGlob}/$1/sync.git" "sandbox/$1:sandbox/$1"
        echo "fetched sandbox/$1"
      '';
    };
    "sandbox:stop" = {
      description = "Stop a running instance: sandbox-stop <instance>";
      command = ''
        rt="''${XDG_RUNTIME_DIR:-/tmp}/katsuobushi/${projectId}/$1"
        sock="$rt/katsuobushi.sock"
        if [ -S "$sock" ]; then
          # QMP requires capability negotiation before any command.
          { echo '{"execute":"qmp_capabilities"}'; echo '{"execute":"quit"}'; sleep 1; } \
            | ${pkgs.socat}/bin/socat - "UNIX-CONNECT:$sock" >/dev/null 2>&1 || true
        fi
        echo "requested stop for $1"
      '';
    };
  };
in
{
  # `nix run .#sandbox` needs an app; lifecycle helpers are menu commands.
  apps.sandbox = {
    type = "app";
    program = "${sandboxRunner}/bin/sandbox";
    meta.description = "Launch an ephemeral Katsuobushi agent sandbox VM";
  };

  inherit menuCommands;

  # Building the guest image so CI catches a broken sandbox config (§5).
  checks.sandbox = runner;

  # The assembled guest system, exposed for advanced/inspection use (§5).
  nixosConfiguration = guestSystem;
}
