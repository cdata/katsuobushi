# Katsuobushi agent sandbox.
#
# Assembles a `microvm.nix` guest — a real NixOS system booted under qemu with a
# genuine kernel boundary — that comes up as a working local dev environment in
# which an agent harness can run with its blast radius bounded by the VM rather
# than by host permission prompts. The harness and any tooling it needs are
# supplied by the caller via `packages`; nothing here is tied to a specific
# agent.
#
# Like the other Katsuobushi libraries this module is partial-applied by the
# flake with its pinned `microvm` dependency; the resulting function is what a
# consumer calls as `katsuobushi.lib.sandbox { inherit pkgs; ... }`. `microvm`
# is exposed as an optional argument so it stays overridable per-call.
#
# The whole VM is hermetic: the proxy, firewall, allowlist, DNS policy, and
# agent environment are baked into the guest image and enforced by guest init.
# The host runs only one `qemu` process per instance — there is no shared
# host-side daemon — which is what makes running many instances (and many
# projects, each with its own allowlist) trivial. Per-instance dynamic values
# (the bare-mirror path, the ssh port, secret files) are emitted into the qemu
# invocation at launch, so nothing instance-specific is baked into the store.
defaults:
{
  pkgs,
  # Path to the consumer's project (e.g. `./.`). Used at launch by the host
  # runner to build the per-instance bare mirror; not baked into the image.
  workspaceRoot,
  # Stable, owner-qualified identifier (e.g. "cdata/katsuobushi"). Names the
  # well-known in-guest project path and the per-instance host state dirs.
  projectId,

  # Network egress
  #
  # Extra reachable origins, appended to `baseAllowedOrigins`. Hostnames only;
  # each becomes a squid `dstdomain`. No implicit wildcards — "github.com"
  # matches only that host; ".github.com" opts into the subtree.
  allowedOrigins ? [ ],
  # The lean baseline every sandbox gets. There is deliberately no per-entry
  # subtraction; to "remove" a baseline host, override this list wholesale.
  baseAllowedOrigins ? [
    # Anthropic inference. CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 (set in the
    # guest env) keeps Claude Code's telemetry/feature-flag/autoupdate hosts off
    # the allowlist — the single biggest lever for a small baseline. NB: agent
    # mode UNSETS that var (the channels feature is gated behind a flag fetch it
    # suppresses), so a dormant agent will *attempt* those ancillary hosts; the
    # proxy still denies any not on this list, and channels were validated to
    # work with this lean baseline unchanged.
    "api.anthropic.com"
    # OAuth/subscription auth validation. Claude Code contacts this even with
    # nonessential traffic disabled; without it auth fails with ERR_BAD_REQUEST.
    "platform.claude.com"
    # Nix: substituters + the GitHub flake-input hosts. The minimum for in-guest
    # `nix develop` plus typical github flake inputs. Trim per-host if your flake
    # has no github inputs.
    "cache.nixos.org"
    "channels.nixos.org"
    "github.com"
    "api.github.com"
    "codeload.github.com"
  ],

  # Reference repos: build-time pinned, writable copies
  #
  # List of { source, dest }. `source` is a store path / derivation (a flake
  # input with `flake = false`, or a `pkgs.fetchFromGitHub { ... }`). `dest` is
  # relative to the agent home; mirror the host Git-layout convention, e.g.
  # "Git/github.com/oxalica/rust-overlay".
  extraRepos ? [ ],

  # Untracked workspace context: project-scoped, one-way host->guest.
  #
  # List of project-relative paths (e.g. ".claude", "notes") carried into the
  # workspace on top of the mirror clone. Absolute paths and ".." are rejected
  # at eval time; symlink escape is refused at copy time by the host runner.
  workspaceContext ? [ ],

  # Home-file mappings: dest (in agent home) -> { source, path?, mode }
  #
  # `source` is a store path; `path` an optional subpath within it. Modes:
  #   "immutable" — read-only bind mount; fixed even against the agent.
  #   "seed"      — copied into home at boot; the agent may edit it.
  #   "link"      — store symlink; present but replaceable (cheapest).
  homeFiles ? { },

  # Runtime secrets: NAME -> { fromEnv = "VAR"; } | { fromFile = "P"; }
  #
  # The declaration lives here; the value is read from the host by the runner at
  # launch and injected via fw_cfg (never in the store, argv, or on disk). The
  # runner fails fast if a declared secret is missing.
  secrets ? { },

  # Resources
  vcpu ? 4,
  mem ? 8192, # MiB. NB: qemu hangs at exactly 2048 (microvm.nix#171).

  # Writable scratch is disk-backed, not RAM-backed. Three sparse raw images
  # (created lazily, sized in MiB) replace the old tmpfs overlays so heavy
  # builds — Rust `target/` dirs especially — spill to host disk instead of
  # competing for the guest's RAM. Provision generously: the images are sparse
  # and trimmed (discard), so host usage tracks real content, not these caps.
  storeVolumeSize ? 16384, # writable /nix/store overlay (in-guest `nix build`)
  scratchVolumeSize ? 32768, # workspace clone + cargo/rustup/XDG caches
  dbVolumeSize ? 4096, # guest Nix database (importHostStoreDb)

  # Packages to put on the guest's PATH. This is where the agent harness goes —
  # it is just another package, not a built-in concept, so the consumer supplies
  # it (and any extra tooling) here. For Claude Code, pass nixpkgs' `claude-code`
  # (unfree — the consumer's `pkgs` must allow it) or a flake's build of it; see
  # templates/sandbox. For arbitrary NixOS config beyond packages, use
  # `guestModules`.
  packages ? [ ],

  # Import the host's Nix database into the guest, so in-guest `nix develop` /
  # `nix build` reuse everything the host has already built instead of
  # re-downloading it.
  #
  # The host `/nix/store` is already shared into the guest read-only, but
  # microvm.nix's guest Nix database only knows the VM's *system* closure — every
  # other host path is present as bytes on the 9p mount yet not registered as
  # valid, so the guest's `nix` ignores it and re-substitutes from the network.
  # With this on, the runner snapshots the host's `db.sqlite` at launch (a
  # consistent SQLite `.backup`, ~0.5s) into the per-instance share, and a guest
  # boot service transplants it over the system-only DB *after* microvm's own
  # closure registration. Because the guest system closure was itself built on the
  # host, the host DB is a strict superset, so the swap keeps the VM bootable
  # while marking all host paths valid — served straight from the shared store
  # with zero network and zero copying. Only genuinely host-absent paths then hit
  # the network, and only if their origin is allowlisted.
  #
  # The transplant is best-effort: if the snapshot is missing or the swapped DB
  # fails a sanity check (e.g. a host/guest Nix schema mismatch), the guest rolls
  # back to its system-only DB, so a sandbox always boots. No new read exposure:
  # the whole host store is already readable over the ro mount; this only changes
  # what `nix` treats as valid.
  importHostStoreDb ? true,

  # Graphics (opt-in): headless GPU rendering for browser tests and Wayland apps.
  #
  # Disabled by default — a graphics-off instance is byte-for-byte what ships
  # today (no spec `graphics` key, no GPU args). When `enable`, the host renders
  # the spec's `graphics` block (camelCase: enable, gpu[], output{width,height,
  # refresh}) and (once the runner resolves a GPU rung at launch) splices a
  # `virtio-gpu-gl` device + headless EGL backend into qemu.
  #   gpu    — ordered role-preference list, resolved host-side; the first
  #            present+openable rung wins. The `software` tail is the llvmpipe
  #            (Tier A) floor that removes the GPU device entirely.
  #   output — the headless sway virtual output (the guest display stack lands
  #            separately; this only carries the dimensions through the spec).
  # Partial attrsets are accepted: anything unset falls back to the defaults
  # below (so `graphics = { enable = true; }` gets the full default gpu/output).
  graphics ? {
    enable = false;
  },

  # Escape hatch: extra NixOS modules merged into the guest.
  guestModules ? [ ],

  # Infra dependency, defaulting to the version Katsuobushi pins.
  microvm ? defaults.microvm,
  # The lib.rust helper and Katsuobushi's own workspace source, used to build
  # the in-tree host↔guest sandbox controller crate that powers agent mode. Both are
  # partial-applied by the Katsuobushi flake; a consumer never sets them.
  rust ? defaults.rust,
  controlSrc ? defaults.controlSrc,
}:

let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;

  # Bare project name (drops the owner qualifier), used for the well-known path.
  projectName = baseNameOf projectId;

  # Effective, fully-resolved egress allowlist. The manifest always prints this
  # so the agent and the human see exactly what is reachable.
  effectiveAllowedOrigins = baseAllowedOrigins ++ allowedOrigins;

  # In-guest constants.
  agentUser = "agent";
  agentHome = "/home/${agentUser}";
  # Writable scratch lives on a disk-backed volume mounted here, not on the RAM
  # root tmpfs: the workspace clone (with its build artifacts) and the agent's
  # build caches go on disk so a Rust `target/` can't exhaust guest RAM.
  scratchMount = "/scratch";
  workspaceParent = "${scratchMount}/workspace";
  workspacePath = "${workspaceParent}/${projectName}";
  # Build caches relocated onto the scratch volume via the agent's environment.
  cargoHome = "${scratchMount}/cargo";
  rustupHome = "${scratchMount}/rustup";
  xdgCacheHome = "${scratchMount}/cache";
  # Volume-backed mount points + their by-label device names.
  rwStoreMount = "/nix/.rw-store";
  nixDbMount = "/nix/var/nix/db";
  rwStoreLabel = "katsu-rwstore";
  nixDbLabel = "katsu-nixdb";
  scratchLabel = "katsu-scratch";
  proxyPort = 3128;
  proxyUid = 3128; # fixed so the nftables uid match is deterministic.
  guestMac = "02:00:00:00:00:02";
  # The whole per-instance host state dir is exposed to the guest as a single 9p
  # share — exactly one host directory the guest can write to. It holds sync.git,
  # context/, console.log, and the small non-secret runtime files (instance
  # name, mode, task, authorized_keys). Secrets never go here.
  shareTag = "katsuobushi";
  shareMount = "/mnt/katsuobushi";
  # slirp user-mode networking puts the built-in DNS forwarder here. squid (and
  # only squid) is allowed to use it; the agent gets no resolver at all.
  slirpDns = "10.0.2.3";

  # Liveness tunables — the single source for both sides. Rendered into the host
  # spec (katsuctlSpec, specVersion 3) and, for the two the guest reads directly,
  # into the agent env. Inert knobs until a consumer reads them.
  heartbeatSecs = 10; # heartbeat cadence (H)
  heartbeatMiss = 3; # dead after N·H = 30 s of silence (N)
  progressStallSecs = 300; # surface "no progress" (no break)
  deliveryDeadlineSecs = 20; # resend if no TurnAccepted
  deliveryRetries = 3; # max resends, then fail (K)
  readyGateSecs = 60; # wait for SessionReady, then send anyway (G)
  stopGraceMs = 1500; # absorb a late terminal report after Stop

  secretNames = builtins.attrNames secrets;

  # Graphics config, normalized over the defaults. `recursiveUpdate` merges a
  # partial consumer attrset (e.g. `{ enable = true; }`, or one that overrides
  # only `output.width`) onto the defaults; the `gpu` list is replaced wholesale,
  # never element-merged, which is what an ordered preference list wants.
  graphicsDefaults = {
    enable = false;
    gpu = [
      "integrated"
      "discrete"
      "software"
    ];
    output = {
      width = 1920;
      height = 1080;
      refresh = 60;
    };
  };
  graphicsCfg = lib.recursiveUpdate graphicsDefaults graphics;

  # The QEMU that actually launches a graphics instance — it must be the
  # opengl/virgl-enabled build, since microvm's default strips that. microvm's qemu
  # runner (lib/runners/qemu.nix) wraps `microvm.qemu.package` in
  # `minimizeQemuClosureSize`, which — whenever `microvm.optimize.enable` (the
  # default) and microvm's OWN `graphics.enable` is off — does
  # `qemu.override { nixosTestRunner = true; }`. In nixpkgs that flag flips the
  # GL feature defaults off (`sdlSupport`/`openGLSupport ? … && !nixosTestRunner`,
  # `virglSupport ? openGLSupport`), yielding a `qemu-*-for-vm-tests` build whose
  # only `-display` backends are `none`/`dbus` and whose virtio-gpu is plain
  # `virtio-gpu-pci` — so a graphics launch dies at
  #   -display egl-headless,…: Parameter 'type' does not accept value 'egl-headless'
  # (caught only on a real boot — eval/build can't see it). We can't avoid
  # the strip without microvm's `graphics.enable` (which forces a gtk window, no
  # headless) or disabling `optimize` (broad boot-config changes). Instead PIN the
  # two GL flags on the package so the chained `nixosTestRunner = true` override
  # can't reset them: `egl-headless` needs `openGLSupport`, `virtio-gpu-gl-pci`
  # needs `virglSupport`. Based on the OUTER `pkgs.qemu_kvm` (host-cpu-only, KVM,
  # smaller than the full qemu); captured before the guestModule's `pkgs` arg
  # shadows it. NB: this is a non-cached variant → a one-time from-source build.
  graphicsQemu = pkgs.qemu_kvm.override {
    openGLSupport = true;
    virglSupport = true;
  };

  # Headless-sway config (referenced only when graphics.enable). One virtual
  # output sized from graphicsCfg.output; sway is launched with `-c` so ONLY this
  # file loads — no default keybindings/bar, because this is a headless render
  # target, not an interactive desktop. The `WLR_HEADLESS_OUTPUTS=1` backend
  # creates exactly one output named `HEADLESS-1`; we set its mode here. sway
  # deterministically binds the `wayland-1` socket (its socket loop starts at 1
  # and skips `wayland-0` — sway/server.c), which is the stable WAYLAND_DISPLAY
  # the agent env/ssh export below advertise.
  swayOutput = "HEADLESS-1";
  swayConfig = pkgs.writeText "katsuobushi-sway.conf" ''
    output ${swayOutput} mode ${toString graphicsCfg.output.width}x${toString graphicsCfg.output.height}@${toString graphicsCfg.output.refresh}Hz
    output ${swayOutput} dpms on
  '';

  # In-tree sandbox controller crate (agent mode)
  #
  # Built reproducibly via lib.rust/crane from Katsuobushi's own workspace
  # source. The server + `report` binaries are baked into the guest; the host
  # client now lives in `katsuctl sandbox prompt`. See.
  controlRust = rust {
    inherit pkgs;
    workspaceRoot = controlSrc;
    projectId = "cdata/katsuobushi";
  };
  # Guest controller server (the host client was retired into katsuctl).
  controlPkg = controlRust.buildCrate {
    pname = "katsuobushi-sandbox-guest";
    cargoExtraArgs = "--package katsuobushi-sandbox-guest";
  };
  # Host-side controller, built from the same workspace.
  # `nix run .#sandbox` runs outside the devshell, where `katsuctl` is not on
  # PATH, so apps.sandbox references this binary explicitly (and puts it on PATH
  # so the emitted agent-start tail-call `exec katsuctl … prompt` resolves too).
  katsuctlPkg = controlRust.buildCrate {
    pname = "katsuctl";
    cargoExtraArgs = "--package katsuobushi-controller";
  };

  # vsock + control-socket constants. The guest server listens on AF_VSOCK; the
  # `report` command and server rendezvous on a guest-local unix socket under a
  # per-agent dir in the RAM-backed /run. The host vsock port is fixed (the
  # per-instance discriminator is the CID, emitted at launch).
  controlSocketDir = "/run/katsuobushi/control";
  reportSocket = "${controlSocketDir}/report.sock";
  controlServerBin = "${controlPkg}/bin/katsuobushi-sandbox-guest";

  # The agent's `report` command — a shell app, not a Rust binary. It just
  # writes one JSON line (the ReportLine wire shape) to the server's guest-local
  # unix socket; jq guarantees correct escaping of arbitrary status text, socat
  # carries the line. Opaque to the agent.
  reportApp = pkgs.writeShellApplication {
    name = "report";
    runtimeInputs = with pkgs; [
      jq
      socat
    ];
    text = ''
      if [ "''${1:-}" = "hook" ]; then
        event="''${2:-}"
        case "$event" in
          session-ready | turn-accepted | turn-ended) ;;
          *)
            echo "usage: report hook <session-ready|turn-accepted|turn-ended>" >&2
            exit 2
            ;;
        esac
        # turn-ended → turnended (HookEvent serializes rename_all = "lowercase")
        ev="$(printf '%s' "$event" | tr -d '-')"
        printf '%s\n' "$(jq -nc --arg e "$ev" '{event:$e}')" \
          | socat - "UNIX-CONNECT:''${KATSU_REPORT_SOCK:-${reportSocket}}" || true
        exit 0
      fi
      status="''${1:-}"
      text="''${2:-}"
      turn="''${3:-}"
      case "$status" in
        working | done | blocked | info) ;;
        *)
          echo "usage: report <working|done|blocked|info> <text> [turn_id]" >&2
          exit 2
          ;;
      esac
      if [ -z "$text" ]; then
        echo "usage: report <working|done|blocked|info> <text> [turn_id]" >&2
        exit 2
      fi
      if [ -n "$turn" ]; then
        line="$(jq -nc --arg s "$status" --arg t "$text" --argjson id "$turn" \
          '{status:$s,text:$t,turn_id:$id}')"
      else
        line="$(jq -nc --arg s "$status" --arg t "$text" '{status:$s,text:$t}')"
      fi
      printf '%s\n' "$line" \
        | socat - "UNIX-CONNECT:''${KATSU_REPORT_SOCK:-${reportSocket}}"
    '';
  };

  # Eval-time validation of consumer-supplied relative paths
  #
  # One predicate for every path that is joined under a trusted root
  # (workspaceContext under the project, extraRepos/homeFiles dests under the
  # agent home): absolute paths and every ".."-escape form are rejected.
  # `what` names the offending option in the error. Shared so the four
  # traversal checks cannot drift apart (checkRepoDest historically missed the
  # `/..`-suffix form).
  checkRelativePath =
    what: root: p:
    if lib.hasPrefix "/" p then
      throw ''
        katsuobushi.lib.sandbox: ${what} "${p}" must be a path relative to
        ${root}, not an absolute path.''
    else if
      (p == "..") || (lib.hasPrefix "../" p) || (lib.hasInfix "/../" p) || (lib.hasSuffix "/.." p)
    then
      throw ''
        katsuobushi.lib.sandbox: ${what} "${p}" must not escape ${root}
        with "..".''
    else
      p;
  validatedContext = map (
    checkRelativePath "workspaceContext entry" "the project root"
  ) workspaceContext;

  checkRepoDest =
    r: r // { dest = checkRelativePath "extraRepos dest" "the agent home" r.dest; };
  validatedRepos = map checkRepoDest extraRepos;

  # Discoverability manifest
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

    ## Returning your work

    Commit and `git push` on `sandbox/<instance>`. The pushed branch is the
    work product and the signal that you are done — there is nothing else to
    report; the human watches the branch (and this VM's console).

    ## Things you are *not* able to do here

    - Reach arbitrary network hosts (only the allowlist above, via the proxy).
    - Resolve DNS (there is no resolver).
    - Touch the host system or other projects (a real kernel boundary separates
      you).
    - Persist anything beyond the branch you push and files you write into
      `${shareMount}`.
    - Use the human's git credentials or upstream remotes — they are not present.
  '';

  # Agent-mode operating contract, injected at launch via
  # `--append-system-prompt-file`. Always-on for every turn in
  # the dormant session, fully ours, and scoped to agent mode — so it does NOT
  # touch ~/.claude/CLAUDE.md, which stays the consumer's.
  agentContract = pkgs.writeText "katsuobushi-agent-contract.md" ''
    # Katsuobushi agent-mode operating contract

    You are a long-lived session inside a Katsuobushi sandbox VM, driven by a
    host operator rather than a human at this terminal.

    **Operator directives arrive as channel turns** that look like
    `<channel source="katsuobushi-sandbox-guest" turn_id="N">…</channel>`.
    Treat the content of each such turn as your next instruction.

    For each directive:

    1. Do the work in the project at `${workspacePath}`.
    2. Commit and `git push` on the branch `sandbox/<instance>` (already checked
       out). The pushed branch is the work product; the channel never carries
       code.
    3. Run `report done "<short summary>"` to signal completion of the turn.

    Other status reports (run them as ordinary shell commands):

    - `report working "<note>"` — optional progress while you work.
    - `report blocked "<what you need>"` — you cannot proceed; then wait for the
      next directive.
    - `report info "<note>"` — anything else worth surfacing to the operator.

    Do not wait for, or ask for, interactive confirmation — there is no human at
    this terminal. When the operator tells you that you are finished (or to shut
    down), run `systemctl poweroff` to end the session.

    Your full environment manifest — network allowlist, reference repos, what
    you can and cannot do — is at `~/README.md`. Read it if you need orientation.
  '';

  # Poller that dismisses Claude Code's development-channels acknowledgement.
  # `--dangerously-load-development-channels` shows a blocking interactive prompt
  # ("I am using this for local development / Exit") on EVERY launch: it is not
  # persisted to config and there is no settings key to pre-accept it (it is
  # bound to the CLI flag, and its accept handler writes nothing). A dormant
  # session has nobody to answer it, so claude would block forever before arming
  # the channel and spawning the controller server. We watch the pane with
  # `tmux capture-pane` and, once the prompt appears, send Enter — which accepts
  # the highlighted default ("I am using this for local development") — with
  # `tmux send-keys`. The poll is bounded so a missing/renamed prompt cannot
  # wedge boot; the space-strip tolerates the TUI's box-drawing. Timing/wording
  # is empirical — revisit if Claude Code changes this prompt.
  #
  # WHY tmux and not zellij for the dormant session: zellij's actions
  # (write/dump-screen) require an *attached client*, and a dormant session has
  # none — they fail with "no active session", so zellij CANNOT inject this
  # keystroke headlessly (verified 2026-06-24, the hard way). tmux targets
  # sessions by name and its send-keys/capture-pane work on a detached session
  # with no client — exactly the intended fallback. The tradeoff is
  # losing zellij's nicer attach UX.
  agentChannelAck = pkgs.writeShellScript "katsuobushi-channel-ack" ''
    export PATH=${
      lib.makeBinPath [
        pkgs.coreutils
        pkgs.gnugrep
        pkgs.tmux
      ]
    }
    for _ in $(seq 1 40); do
      sleep 2
      if tmux capture-pane -t katsuobushi -p 2>/dev/null | tr -d ' ' | grep -qi 'forlocaldevelopment'; then
        tmux send-keys -t katsuobushi Enter
        # Probe-independent SessionReady arming signal: the dev-
        # channels prompt has just been dismissed, so the session is live —
        # emit session-ready directly rather than relying solely on the
        # SessionStart hook. Best-effort; never wedge boot on the report socket.
        ${reportApp}/bin/report hook session-ready || true
        break
      fi
    done
  '';

  # homeFiles always includes the generated manifest as an internal immutable
  # entry at ~/README.md. We deliberately do NOT own ~/.claude/CLAUDE.md: that
  # file is reserved for the consumer (e.g. a universal AGENTS.md mapped via
  # homeFiles), so the lib must never squat it. The manifest is surfaced to the
  # in-VM agent by other means — the interactive login shell cats it (see
  # loginShellInit), and agent mode injects a pointer to it via
  # `--append-system-prompt-file` at launch.

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
  #
  # We also register the sandbox controller server here, at **user scope** (top-level
  # `mcpServers`). This is deliberate: Claude Code's "New MCP server found in
  # this project — Use this MCP server?" consent gate fires only for *project*
  # `.mcp.json` servers, and that dialog does not relay — a dormant headless
  # agent has nobody to accept it. A user-scoped server is pre-trusted, so the
  # channel registers unattended. `--dangerously-load-development-channels
  # server:katsuobushi-sandbox-guest` (passed only in agent mode) resolves to
  # this entry by name.
  claudeConfigSeed = pkgs.writeText "claude.json" (
    builtins.toJSON {
      hasCompletedOnboarding = true;
      theme = "dark";
      mcpServers = {
        katsuobushi-sandbox-guest = {
          command = controlServerBin;
          args = [ ];
          env.KATSU_REPORT_SOCK = reportSocket;
        };
      };
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

  effectiveHomeFiles =
    defaultHomeFiles
    // homeFiles
    // {
      "README.md" = {
        source = manifest;
        mode = "immutable";
      };
    };

  # Resolve a homeFiles entry to the concrete source file path in the store.
  homeFileSource =
    entry:
    if entry ? path && entry.path != null then "${entry.source}/${entry.path}" else "${entry.source}";

  # Validate at eval time: an unknown mode would otherwise fall through every
  # filter below and the file would silently never appear in the guest — a
  # typo'd "immutible" deserves a loud failure, not a missing file. The dest
  # gets the same traversal check as the other home-rooted paths.
  homeFileModes = [
    "immutable"
    "seed"
    "link"
  ];
  homeFilesList = lib.mapAttrsToList (
    dest: entry:
    if !(builtins.elem (entry.mode or null) homeFileModes) then
      throw ''
        katsuobushi.lib.sandbox: homeFiles."${dest}" has unknown mode
        "${toString (entry.mode or "<unset>")}"; expected one of
        ${lib.concatStringsSep " | " homeFileModes}.''
    else
      {
        dest = checkRelativePath ''homeFiles dest'' "the agent home" dest;
        src = homeFileSource entry;
        mode = entry.mode;
      }
  ) effectiveHomeFiles;

  immutableHomeFiles = builtins.filter (e: e.mode == "immutable") homeFilesList;
  seedHomeFiles = builtins.filter (e: e.mode == "seed") homeFilesList;
  linkHomeFiles = builtins.filter (e: e.mode == "link") homeFilesList;

  # Squid forward-proxy configuration
  #
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

  # Host runner: launch-time argument emission
  #
  # microvm.nix runs this at launch and splices its single line of stdout into
  # the qemu invocation. It reads only env/paths prepared by the wrapper, so
  # nothing instance-specific is in the store.
  extraArgsScript = pkgs.writeShellScript "katsuobushi-extra-args" ''
    set -eu
    args=""

    # User-mode (slirp) NIC with an ssh hostfwd bound to host loopback only.
    # passt is unsupported by microvm.nix's qemu runner, so we use the
    # guaranteed slirp fallback.
    args="$args -netdev user,id=net0,hostfwd=tcp:127.0.0.1:''${KATSU_SSH_PORT}-:22"
    args="$args -device virtio-net-pci,netdev=net0,mac=${guestMac},romfile="

    # Per-instance state dir as one rw 9p share.
    args="$args -fsdev local,id=katsushare,path=''${KATSU_STATE_DIR},security_model=mapped-xattr"
    args="$args -device virtio-9p-pci,fsdev=katsushare,mount_tag=${shareTag}"

    # Agent mode: a vhost-vsock device with the per-instance CID the runner
    # allocated, for the host↔guest controller channel. Emitted only when
    # a CID is present; interactive instances get no vsock device at all.
    if [ -n "''${KATSU_VSOCK_CID:-}" ]; then
      args="$args -device vhost-vsock-pci,guest-cid=''${KATSU_VSOCK_CID}"
    fi

    # Graphics (opt-in): when the runner resolved a GPU rung it exports
    # KATSU_GFX_RENDERNODE (and KATSU_GFX_VENUS=1 for the venus path), so splice a
    # virtio-gpu-gl device + a headless EGL backend bound to that render node. The
    # `software` rung and a graphics-off instance export neither var, so no GPU
    # device is emitted and the boundary is unchanged.
    # `-sandbox on` (qemu seccomp) is cheap defense-in-depth for the widened GPU
    # surface; confirmed not to break microvm boot on a real boot.
    # The venus (Vulkan) path additionally requires `blob=true` + a `hostmem`
    # window — without them qemu refuses the device: "venus requires enabled blob
    # and hostmem options" (found on a real boot). 8G matches the recommended
    # 8 GiB mem floor; it is a host memory *window* for blob resources, not a
    # guest alloc.
    if [ -n "''${KATSU_GFX_RENDERNODE:-}" ]; then
      venus=""; [ -n "''${KATSU_GFX_VENUS:-}" ] && venus=",venus=on,blob=true,hostmem=8G"
      args="$args -device virtio-gpu-gl-pci$venus"
      args="$args -display egl-headless,rendernode=''${KATSU_GFX_RENDERNODE}"
      args="$args -sandbox on"
    fi

    # Declared secrets via fw_cfg, reading from tmpfs files the wrapper staged.
    ${lib.concatMapStrings (name: ''
      args="$args -fw_cfg name=opt/io.systemd.credentials/${name},file=''${KATSU_CRED_${name}}"
    '') secretNames}

    printf '%s' "$args"
  '';

  # The guest NixOS system
  guestModule =
    {
      config,
      lib,
      pkgs,
      utils,
      ...
    }:
    let
      # The systemd mount unit for a mount point, via the canonical escaping
      # (NixOS' own `escapeSystemdPath`, not a hand-rolled replaceStrings —
      # the two differ on `-` and other special characters).
      mountUnit = path: "${utils.escapeSystemdPath path}.mount";
    in
    {
      # Boot/runner shape.
      microvm = {
        hypervisor = "qemu";
        # QEMU package. When graphics is on, use the GL-flag-pinned
        # `graphicsQemu` (see above) so microvm's closure-minimizer can't strip
        # `egl-headless`/`virtio-gpu-gl-pci`. `mkIf` so a graphics-off guest keeps
        # microvm's lean default, byte-for-byte.
        qemu.package = lib.mkIf graphicsCfg.enable graphicsQemu;
        inherit vcpu mem;
        # No static interfaces: the NIC (with its per-instance hostfwd port) is
        # emitted by extraArgsScript so parallel instances do not collide.
        interfaces = [ ];
        extraArgsScript = "${extraArgsScript}";
        # Shared host /nix/store (ro) + a writable overlay so in-guest `nix
        # develop` builds work.
        shares = [
          {
            tag = "ro-store";
            source = "/nix/store";
            mountPoint = "/nix/.ro-store";
            proto = "9p";
          }
        ];
        writableStoreOverlay = rwStoreMount;

        # Disk-backed writable scratch (replaces the old RAM-backed tmpfs
        # overlays). Three sparse raw images, created+mkfs'd on the host at
        # launch and mounted by label. The images are symlinked into the
        # per-instance state dir by the runner (see the launch flow), so a named
        # instance's build caches survive a stop/restart while ephemeral ones are
        # cleaned up with the rest of its state.
        #
        # microvm forces neededForBoot on whichever volume backs
        # writableStoreOverlay (it is the /nix/store overlay's upperdir), so the
        # rw-store image is mounted in the initrd; the other two mount normally
        # post-switch-root, which is early enough (the nix-db seed and the
        # workspace clone are both ordered after their mounts).
        volumes = [
          {
            image = "rw-store.img";
            label = rwStoreLabel;
            mountPoint = rwStoreMount;
            size = storeVolumeSize;
            fsType = "ext4";
          }
          {
            image = "nix-db.img";
            label = nixDbLabel;
            mountPoint = nixDbMount;
            size = dbVolumeSize;
            fsType = "ext4";
          }
          {
            image = "scratch.img";
            label = scratchLabel;
            mountPoint = scratchMount;
            size = scratchVolumeSize;
            fsType = "ext4";
          }
        ];
      };

      # Online discard on the volumes so freed blocks return to the host and the
      # sparse images track real content rather than creeping to their nominal
      # size. microvm already passes `discard=unmap` on the virtio-blk drives;
      # this is the guest half. (device/fsType/neededForBoot come from microvm's
      # volume-derived fileSystems entries; we only add mount options here.)
      fileSystems.${rwStoreMount}.options = [ "discard" ];
      fileSystems.${nixDbMount}.options = [ "discard" ];
      fileSystems.${scratchMount}.options = [ "discard" ];

      # Relocate the agent's build caches onto the scratch volume. A login shell
      # (the agent and ssh sessions both use one) sources these from /etc/profile.
      environment.sessionVariables = {
        CARGO_HOME = cargoHome;
        RUSTUP_HOME = rustupHome;
        XDG_CACHE_HOME = xdgCacheHome;
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

      # virtio-vsock guest transport, for the host↔guest controller channel in agent
      # mode. The matching `vhost-vsock-pci` device is emitted at
      # launch (extraArgsScript) only when a CID is allocated; loading the
      # module unconditionally is harmless when no device is present.
      #
      # Graphics: the PCI `virtio-gpu-gl` device the host splices in is normally
      # auto-probed by the in-tree `virtio_gpu` DRM driver, but pin it so `card0`/
      # `renderD128` are guaranteed present for sway/Mesa regardless of probe
      # order. Appended only when graphics.enable, so a graphics-off
      # guest's module list is byte-for-byte unchanged.
      boot.kernelModules = [
        "vmw_vsock_virtio_transport"
      ]
      ++ lib.optionals graphicsCfg.enable [ "virtio_gpu" ];

      # Graphics: Mesa userspace for the guest GPU — the virtio Vulkan ICD
      # (`libvulkan_virtio.so`), the `virtio_gpu` Gallium GL driver, and llvmpipe
      # for the `software` rung. `mkIf` so a graphics-off guest
      # never pulls in `hardware.graphics` (default off ⇒ identical eval).
      hardware.graphics.enable = lib.mkIf graphicsCfg.enable true;

      # Headless sway — the guest display stack (wlroots family)
      #
      # A systemd *user* service under the unprivileged agent (NOT a system
      # service) so the compositor stays inside the agent's blast radius —
      # consistent with "no root, no sudo." The agent lingers (see
      # users.users.${agentUser}.linger) so this starts at boot with no login and
      # is live before the first ssh/`grim` or workload. `WLR_BACKENDS=headless` +
      # `WLR_HEADLESS_OUTPUTS=1` create one virtual output (`HEADLESS-1`, sized by
      # swayConfig from graphicsCfg.output); on a GPU rung wlroots renders through
      # the virtio render node, on the `software` rung through llvmpipe. sway
      # binds the `wayland-1` socket the env/ssh exports advertise. Whether it
      # brings the compositor up and exports a usable display is confirmed by
      # end-to-end validation on a real boot (this VM has no nested KVM); here it
      # only needs to evaluate and build.
      systemd.user.services.katsuobushi-sway = lib.mkIf graphicsCfg.enable {
        description = "Katsuobushi headless sway compositor (agent display stack)";
        wantedBy = [ "default.target" ];
        environment = {
          WLR_BACKENDS = "headless";
          WLR_HEADLESS_OUTPUTS = "1";
          # No physical input on a headless seat; don't probe libinput devices.
          WLR_LIBINPUT_NO_DEVICES = "1";
        };
        serviceConfig = {
          ExecStart = "${pkgs.sway}/bin/sway --config ${swayConfig}";
          # A transient renderer hiccup (e.g. the GPU node not yet settled) should
          # not permanently lose the display.
          Restart = "on-failure";
          RestartSec = 2;
        };
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

      # Users (6.1)
      users.mutableUsers = false;
      # Intentional: there is no root/password login. The agent authenticates
      # with the ephemeral key injected at launch; root is unreachable.
      users.allowNoPasswordLogin = true;
      users.users.${agentUser} = {
        isNormalUser = true;
        home = agentHome;
        # No sudo / wheel: the agent runs strictly unprivileged so the in-guest
        # firewall is a genuine boundary against it. Graphics adds only `video`/
        # `render` so the agent can open the guest DRM nodes (`card0`,
        # `renderD128`) the `virtio_gpu` driver creates — group membership for a
        # device, not a privilege grant; absent when graphics is off.
        extraGroups = lib.optionals graphicsCfg.enable [
          "video"
          "render"
        ];
        # Graphics: linger so the headless-sway *user* service — and the
        # `/run/user/<uid>` runtime dir it (and ssh's `XDG_RUNTIME_DIR`) rely on —
        # come up at boot with no interactive login, so the compositor is live
        # before the first ssh/`grim` or agent workload. `mkIf` so graphics-off
        # leaves linger at its `null` default (an explicit `false` actively manages
        # the marker and would alter the graphics-off build).
        linger = lib.mkIf graphicsCfg.enable true;
      };
      users.users.katsuproxy = {
        isSystemUser = true;
        group = "katsuproxy";
        uid = proxyUid;
      };
      users.groups.katsuproxy.gid = proxyUid;

      # Firewall: nftables default-deny egress (6.3)
      #
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

      # Squid proxy (6.4)
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

      # Agent environment (6.10)
      #
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
        # Code's ancillary hosts off the allowlist. NB: agent mode UNSETS this
        # for the dormant claude (see the agent-mode unit) because Claude Code gates the
        # experimental channels feature behind a feature-flag fetch this var
        # suppresses — with it set, channels report "not currently available"
        # and host->guest prompt injection never reaches claude. Interactive
        # sessions keep this lean, telemetry-off posture.
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "1";
        # Tell Claude Code it is sandboxed. It refuses --dangerously-skip-
        # permissions ("bypass mode") unless it believes it is in a sandbox
        # (IS_SANDBOX=1, or bubblewrap) — which is accurate here (a real VM) —
        # and this keeps bypass mode available for agent mode.
        IS_SANDBOX = "1";
        # Where the controller server listens and the `report` command connects
        # (agent mode). Set globally so both — the server claude spawns, and the
        # report command the agent runs — agree without per-invocation wiring.
        KATSU_REPORT_SOCK = reportSocket;
        # Liveness tunables the guest reads directly; the rest live only in the
        # host spec. KATSU_SHARE is the 9p
        # share mount where the server writes turn-state.json. Plumbed
        # from the same let-bindings as the spec so the two sides can't drift.
        KATSU_HEARTBEAT_SECS = toString heartbeatSecs;
        KATSU_STOP_GRACE_MS = toString stopGraceMs;
        KATSU_SHARE = shareMount;
      };

      # Claude Code "managed settings" — the org-policy settings tier, highest
      # precedence. Two sandbox-forced settings live here (rather than in user
      # settings) so a consumer's own ~/.claude/settings.json mapped via
      # homeFiles cannot accidentally override them, and because managed settings
      # is an accepted source for both keys:
      #   * channelsEnabled — the experimental channels feature is also gated by
      #     org policy ("channels not enabled by org policy"); force it on so the
      #     dormant agent can receive host-injected channel turns.
      #   * skipDangerousModePermissionPrompt — Claude Code shows a blocking
      #     "Bypass Permissions mode" acknowledgement at *interactive* startup
      #     (the old `claude -p` path skipped it). A dormant session has nobody
      #     to accept it, so claude takes the default ("No, exit") and quits —
      #     which is exactly what made the agent's tmux session exit.
      #     This key skips that prompt. NB: the legacy ~/.claude.json
      #     `bypassPermissionsModeAccepted` key does NOT work — Claude migrates
      #     and strips it on startup. (Both validated 2026-06-23.)
      #
      # The `hooks` block wires Claude Code's lifecycle events to the `report
      # hook <event>` subcommand by absolute store path (PATH-independent), so
      # the dormant session emits real liveness lines into the controller server
      # (the server consumes them via GuestLocalLine and drives the turn-state
      # machine). All three live in
      # the managed tier because the probe (real boot 2026-06-28)
      # confirmed managed-settings hooks ARE honored and that Stop, SessionStart,
      # and UserPromptSubmit all fire for injected channel turns — so no
      # degradation/fallback path is active:
      #   * Stop             → turn-ended    (fires once at turn end)
      #   * SessionStart     → session-ready (fires once at claude startup)
      #   * UserPromptSubmit → turn-accepted (fires when a channel turn begins)
      # Schema: each event maps to a list of { hooks = [ { type; command; } ]; }
      # groups; Stop/SessionStart/UserPromptSubmit take no matcher (only tool
      # hooks do).
      environment.etc."claude-code/managed-settings.json".text = builtins.toJSON {
        channelsEnabled = true;
        skipDangerousModePermissionPrompt = true;
        hooks = {
          Stop = [
            {
              hooks = [
                {
                  type = "command";
                  command = "${reportApp}/bin/report hook turn-ended";
                }
              ];
            }
          ];
          SessionStart = [
            {
              hooks = [
                {
                  type = "command";
                  command = "${reportApp}/bin/report hook session-ready";
                }
              ];
            }
          ];
          UserPromptSubmit = [
            {
              hooks = [
                {
                  type = "command";
                  command = "${reportApp}/bin/report hook turn-accepted";
                }
              ];
            }
          ];
        };
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

      # SSH: key-only, agent only, reachable only via the loopback hostfwd
      #
      # The pubkey arrives per-launch through the share.
      services.openssh = {
        enable = true;
        settings = {
          PasswordAuthentication = false;
          KbdInteractiveAuthentication = false;
          PermitRootLogin = "no";
          AllowUsers = [ agentUser ];
        };
        # Graphics: `sandbox screenshot` runs `ssh agent@… 'grim -'`, a
        # non-interactive remote command that does NOT source /etc/profile, so the
        # loginShellInit export below never reaches it. Set WAYLAND_DISPLAY
        # server-side instead (sway binds `wayland-1`), plus DISPLAY=:0 so X11
        # clients reach sway's XWayland (it lazily binds `:0` — the first free X
        # display in this single-compositor guest). XDG_RUNTIME_DIR is already
        # provided to every ssh session by pam_systemd (= /run/user/<uid>, the
        # lingering agent's dir holding the wayland socket), so it needs no help.
        # `mkIf` (not `optionalString ""`) so graphics-off leaves extraConfig
        # unset — an empty string would still append a trailing newline and alter
        # the generated sshd_config.
        extraConfig = lib.mkIf graphicsCfg.enable "SetEnv WAYLAND_DISPLAY=wayland-1 DISPLAY=:0";
      };

      # Login greeting + per-session secret export + env hygiene
      #
      # Added to /etc/profile (NixOS does NOT source /etc/profile.d/*.sh, so this
      # is the hook that actually runs for ssh logins and the autonomous
      # `bash -lc` launcher). Exports each delivered secret as its env var,
      # unsets any stray Anthropic key that would outrank the OAuth token, and
      # prints the manifest on an interactive terminal.
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
      ''
      # Graphics: point every login shell (interactive ssh, and the agent-mode
      # `bash -lc` tmux session) at the headless sway compositor, so a workload is
      # just `firefox` / `my-app` with no ceremony. sway binds `wayland-1`
      # deterministically; DISPLAY=:0 covers X11 clients via sway's XWayland (it
      # lazily binds `:0`, the first free X display here), so a tool that probes
      # DISPLAY — or any X-only app — works without per-invocation ceremony.
      # XDG_RUNTIME_DIR is set by pam_systemd for ssh logins; default it for the
      # `runuser`-spawned agent tmux, where pam does not run — the lingering
      # agent's `/run/user/<uid>` exists from boot. Empty (so the profile is
      # unchanged) when graphics is off.
      + lib.optionalString graphicsCfg.enable ''
        : "''${XDG_RUNTIME_DIR:=/run/user/$(id -u)}"
        export XDG_RUNTIME_DIR
        export WAYLAND_DISPLAY=wayland-1
        export DISPLAY=:0
      '';

      environment.systemPackages =
        (with pkgs; [
          git
          coreutils
          gnutar
          gzip
          rsync
          cacert
          # Agent-mode PTY host for the dormant Claude session.
          # A human can `tmux attach -t katsuobushi` over ssh to watch it work.
          # tmux (not zellij) because its send-keys/capture-pane drive a detached
          # session with no attached client — needed to dismiss Claude Code's
          # dev-channels prompt headlessly (see agentChannelAck).
          tmux
        ])
        ++ [
          # Agent mode: the `report` command on the agent's PATH (opaque to it).
          # The controller server binary is not on PATH — claude spawns it by absolute
          # store path from ~/.claude.json, and that reference pulls it into the
          # guest closure (see claudeConfigSeed).
          reportApp
        ]
        # Graphics (opt-in): the headless compositor (sway, also provides
        # `swaymsg` for the real-boot `get_outputs` check), the screenshot tool the
        # `sandbox screenshot` feature itself shells (grim), the nested
        # micro-compositor for the game path (gamescope), and the X server sway's
        # XWayland shim execs so X11 clients reach `DISPLAY=:0` (sway looks for
        # `Xwayland` on PATH; systemPackages puts it on the user service's PATH).
        # Absent from the closure entirely when graphics is off.
        ++ lib.optionals graphicsCfg.enable (
          with pkgs;
          [
            sway
            grim
            gamescope
            xwayland
          ]
        )
        # Consumer-supplied packages, including the agent harness.
        ++ packages;

      # CA bundle so HTTPS-through-proxy validates.
      security.pki.certificateFiles = [ "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];

      # Agent-mode self-shutdown: a scoped polkit rule lets the otherwise
      # unprivileged agent power off ITS OWN VM — and nothing else — via
      # `systemctl poweroff`. poweroff is orthogonal to the
      # egress firewall the unprivileged-agent design protects, so this does not
      # weaken the threat model: worst case a prompt-injected agent self-DoSes
      # its own sandbox.
      security.polkit.enable = true;
      security.polkit.extraConfig = ''
        polkit.addRule(function(action, subject) {
          if (action.id == "org.freedesktop.login1.power-off" &&
              subject.user == "${agentUser}") {
            return polkit.Result.YES;
          }
        });
      '';

      # Agent-owned scratch: the workspace clone and the relocated build caches,
      # all on the disk-backed scratch volume mounted at ${scratchMount}. The
      # volume's own root stays root-owned; these subdirs are the agent's.
      systemd.tmpfiles.rules = [
        "d ${workspaceParent} 0755 ${agentUser} users - -"
        "d ${cargoHome} 0755 ${agentUser} users - -"
        "d ${rustupHome} 0755 ${agentUser} users - -"
        "d ${xdgCacheHome} 0755 ${agentUser} users - -"
        "d /run/katsuobushi 0755 root root - -"
        # Agent-owned: the controller server (spawned by claude as the agent) binds the
        # report socket here, and the `report` command connects to it.
        "d ${controlSocketDir} 0700 ${agentUser} users - -"
      ]
      # seed homeFiles: copy from store into home, agent may edit.
      ++ map (e: "C ${agentHome}/${e.dest} 0644 ${agentUser} users - ${e.src}") seedHomeFiles
      # link homeFiles: store symlink, replaceable.
      ++ map (e: "L+ ${agentHome}/${e.dest} - - - - ${e.src}") linkHomeFiles;

      # Secret delivery
      #
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

      # Inject the per-launch ssh pubkey
      #
      # The wrapper drops the ephemeral pubkey into the share; install it into
      # the agent's authorized_keys before sshd accepts connections.
      systemd.services.katsuobushi-authkeys = {
        description = "Install the ephemeral agent ssh authorized key";
        wantedBy = [ "multi-user.target" ];
        before = [ "sshd.service" ];
        after = [ (mountUnit shareMount) ];
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

      # Seed the guest Nix database from the host (importHostStoreDb)
      #
      # The guest DB lives on the persistent nix-db volume (mounted at
      # ${nixDbMount}). microvm registers the guest *system* closure into it at
      # boot (store-disk.nix's postBootCommands `nix-store --load-db`, additive
      # and idempotent). The runner has dropped a consistent SQLite `.backup` of
      # the *host* DB into the share; we seed it over the system-only DB so every
      # host-built path the ro store mount exposes becomes valid to nix — no
      # network, just a small file copy. The host DB is a superset of the system
      # closure, so the VM stays bootable.
      #
      # Seed ONCE, gated on a marker on the volume. An ephemeral instance gets a
      # fresh (empty, unmarked) volume each launch, so it seeds every boot as
      # before. A named instance keeps its volume across stop/restart: after the
      # first seed it carries the host superset PLUS whatever the agent built and
      # registered in-VM, so re-seeding would clobber those guest registrations
      # and strand the matching paths sitting in the persistent rw-store overlay.
      # Skipping on the marker keeps the DB consistent with that persistent store
      # — the point of disk-backing it: warm `nix build` results survive a pause.
      # (The trade is that a resumed instance does not pick up host paths built
      # after its first launch; refresh by discarding it with --remove.)
      #
      # Best-effort: a missing snapshot or a failed sanity check falls back to a
      # freshly re-registered system-only DB (from the kernel-cmdline regInfo, the
      # same source postBootCommands uses), so a sandbox always boots. Ordered
      # before nix-daemon and the agent so nothing reads the DB mid-seed, and
      # after the nix-db mount so it operates on the volume, not the shadowed root
      # tmpfs. The sanity check runs with NIX_REMOTE unset so it reads the DB
      # directly instead of waking the (not-yet-started) daemon.
      systemd.services.katsuobushi-nixdb = lib.mkIf importHostStoreDb {
        description = "Seed the guest Nix database from the host";
        wantedBy = [ "multi-user.target" ];
        before = [
          "nix-daemon.service"
          "katsuobushi-workspace.service"
          "katsuobushi-agent.service"
        ];
        after = [
          (mountUnit shareMount)
          (mountUnit nixDbMount)
          "local-fs.target"
        ];
        unitConfig.ConditionPathExists = "${shareMount}/nix-db.sqlite";
        path = [ pkgs.coreutils ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          StandardOutput = "journal+console";
          StandardError = "journal+console";
        };
        script = ''
          dbdir=${nixDbMount}
          db="$dbdir/db.sqlite"
          snap=${shareMount}/nix-db.sqlite
          marker="$dbdir/.katsu-seeded"

          if [ -e "$marker" ]; then
            echo "katsuobushi: guest Nix DB already seeded (persistent volume); keeping it."
            exit 0
          fi

          # Move the postBoot-registered DB aside (rename: instant) so we can roll
          # back; drop any stale WAL/SHM so the seeded file is read clean.
          mv -f "$db" "$db.katsu-orig" 2>/dev/null || true
          rm -f "$db-wal" "$db-shm"
          # Probe with nix's own store path: it is in the guest system closure (so
          # valid in the seeded host superset) and obviously present (it is
          # running), and unlike /run/current-system needs no activation to exist
          # this early. A clean query means the seeded DB reads.
          if cp -f "$snap" "$db" && chmod 0644 "$db" \
            && NIX_REMOTE= ${config.nix.package}/bin/nix-store -q --deriver ${config.nix.package} >/dev/null 2>&1; then
            rm -f "$db.katsu-orig"
            : > "$marker"
            echo "katsuobushi: seeded guest Nix DB from host; host-built paths are reusable offline."
          else
            echo "katsuobushi: host Nix DB seed failed; rebuilding a system-only DB." >&2
            rm -f "$db" "$db-wal" "$db-shm"
            # Re-register the system closure from the kernel-cmdline regInfo (the
            # same registration postBootCommands loads) so the fallback DB is at
            # least valid for the guest's own paths, even on a fresh empty volume
            # where there was no prior DB to roll back to.
            reg="$(sed -n 's/.*regInfo=\([^ ]*\).*/\1/p' /proc/cmdline)"
            if [ -n "$reg" ] && [ -e "$reg/registration" ]; then
              NIX_REMOTE= ${config.nix.package}/bin/nix-store --load-db < "$reg/registration" || true
            elif [ -n "$reg" ] && [ -e "$reg" ]; then
              NIX_REMOTE= ${config.nix.package}/bin/nix-store --load-db < "$reg" || true
            fi
            # Leave the marker unset so a later boot retries the seed.
          fi
        '';
      };

      # Immutable homeFiles: per-file read-only bind mounts
      #
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

      # Workspace materialization
      #
      # Clone the bare mirror (its only remote is the sync point — no host
      # credentials/upstreams leak), check out the seed branch, overlay the
      # untracked context, and lay down the writable reference-repo copies.
      systemd.services.katsuobushi-workspace = {
        description = "Materialize the Katsuobushi workspace";
        wantedBy = [ "multi-user.target" ];
        after = [
          (mountUnit shareMount)
          "katsuobushi-homefiles.service"
        ];
        path = with pkgs; [
          git
          coreutils
          rsync
          jq
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
          # Prefer the consolidated instance.json; fall back to the legacy
          # scalar share file, then a hardcoded default, so both old and new hosts boot.
          instance="$(jq -r '.name // empty' ${shareMount}/instance.json 2>/dev/null || true)"
          [ -n "$instance" ] || instance="$(cat ${shareMount}/instance 2>/dev/null || echo unknown)"

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

          # Writable reference-repo copies.
          ${lib.concatMapStrings (r: ''
            mkdir -p "$(dirname ${agentHome}/${r.dest})"
            if [ ! -e ${agentHome}/${r.dest} ]; then
              cp -rT ${r.source} ${agentHome}/${r.dest}
              chmod -R u+w ${agentHome}/${r.dest}
            fi
          '') validatedRepos}
        '';
      };

      # Agent run mode
      #
      # Always present; no-ops unless launched in agent mode. It starts a
      # *dormant interactive* Claude session inside a detached tmux session as
      # the unprivileged agent, with the controller channel server
      # armed so the host can push prompts into the session over vsock. The
      # session lingers; it ends when the agent runs `systemctl poweroff` (told
      # it is finished) or the host stops the VM. Replaces the old `claude -p`
      # autonomous path, which was doomed by the -p→bare billing shift.
      systemd.services.katsuobushi-agent = {
        description = "Katsuobushi agent-mode session (dormant Claude under tmux)";
        wantedBy = [ "multi-user.target" ];
        after = [
          "katsuobushi-workspace.service"
          "katsuproxy.service"
          "network-online.target"
        ];
        wants = [ "network-online.target" ];
        path = with pkgs; [
          coreutils
          util-linux
          jq
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          StandardOutput = "journal+console";
          StandardError = "journal+console";
        };
        # Create the dormant session detached via `tmux new-session -d`, so a
        # real PTY exists with nobody attached (the TUI stays healthy) and a
        # human can `tmux attach -t katsuobushi` over ssh to watch the agent work
        # live. `tmux new-session -d` daemonizes its server cleanly with no
        # controlling terminal. The command runs under a login shell (bash -lc)
        # so the proxy/secret profile and harness PATH apply, and `unset
        # CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` for the dormant claude only
        # (the global env keeps interactive sessions lean) — load-bearing because
        # Claude Code gates the experimental channels feature behind a feature-
        # flag fetch that var suppresses, so with it set channels report "not
        # currently available" and the host->guest injection silently never
        # reaches claude. `exec` lets claude own the pane.
        # --dangerously-load-development-channels arms the controller channel
        # (the server is registered user-scope in ~/.claude.json, so no MCP
        # consent gate); --dangerously-skip-permissions auto-approves (the VM is
        # the blast-radius boundary); the operating contract is appended to the
        # system prompt.
        #
        # After launching, a detached poller (agentChannelAck) accepts the
        # development-channels prompt; see that script for the full why (and why
        # this is tmux, not zellij). It is setsid-detached so this oneshot
        # returns promptly and systemd does not reap it, and runs as the agent
        # (it owns the tmux session).
        script = ''
          set -u
          # Prefer the consolidated instance.json; fall back to the legacy
          # scalar share file, then a hardcoded default, so both old and new hosts boot.
          mode="$(jq -r '.mode // empty' ${shareMount}/instance.json 2>/dev/null || true)"
          [ -n "$mode" ] || mode="$(cat ${shareMount}/mode 2>/dev/null || echo interactive)"
          if [ "$mode" != "agent" ]; then
            exit 0
          fi
          runuser -u ${agentUser} -- ${pkgs.tmux}/bin/tmux new-session -d -s katsuobushi -x 220 -y 50 \
            ${pkgs.bash}/bin/bash -lc 'unset CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC; cd ${workspacePath} && exec claude --dangerously-skip-permissions --dangerously-load-development-channels server:katsuobushi-sandbox-guest --append-system-prompt-file ${agentContract}'
          setsid runuser -u ${agentUser} -- ${agentChannelAck} >/dev/null 2>&1 &
          # Future: idle backstop — reap a forgotten/wedged session.
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

  # Nix→Rust instance spec
  #
  # One JSON spec rendered at flake-eval time and handed to `katsuctl` via
  # `--config`. Rust owns the schema (rust/katsuctl/src/sandbox/spec.rs — every
  # struct `#[serde(deny_unknown_fields)]`, camelCase keys, specVersion checked
  # on load); this is the single source of truth that must mirror it key-for-key.
  # It is built from the SAME let-bindings the runner uses today (projectId,
  # agentUser, importHostStoreDb, validatedContext, secrets, runner, the pinned
  # tool packages) — no duplicated values — so the spec and the remaining shell
  # can never drift. `roots` carries the `$XDG_*` templates verbatim; `katsuctl`
  # expands them in Rust (resolve_roots) with the same `:-` fallbacks the runner
  # uses, rather than baking a user's absolute home path into the
  # store. `sqlite3` is gated on importHostStoreDb exactly as the runner's
  # runtimeInputs are — null (Tools.sqlite3 = None) when off.
  katsuctlSpec = pkgs.writeText "katsuctl-sandbox-spec.json" (
    builtins.toJSON (
      {
        # Bumped to 4 alongside the Rust SUPPORTED_SPEC_VERSION (added
        # `tools.katsuctl`; 3 added the graphics schema). The `graphics` block is
        # appended below only when graphics.enable; a graphics-off spec omits the
        # key entirely, and katsuctl reads an absent `graphics` as disabled (its
        # `#[serde(default)]`). When this bumps, land the Rust bump in the same
        # change — a stale devshell otherwise fails every launch on the mismatch.
        specVersion = 4;
        inherit projectId agentUser importHostStoreDb;
        # Liveness tunables; inert until a
        # consumer reads them, but plumbed from the one let-binding source so the
        # host spec and the guest env can never drift.
        inherit
          heartbeatSecs
          heartbeatMiss
          progressStallSecs
          deliveryDeadlineSecs
          deliveryRetries
          readyGateSecs
          stopGraceMs
          ;
        roots = {
          stateGlob = "$XDG_STATE_HOME/katsuobushi/${projectId}";
          runtimeGlob = "$XDG_RUNTIME_DIR/katsuobushi/${projectId}";
        };
        tools = {
          git = "${pkgs.git}/bin/git";
          ssh = "${pkgs.openssh}/bin/ssh";
          sshKeygen = "${pkgs.openssh}/bin/ssh-keygen";
          tmux = "${pkgs.tmux}/bin/tmux";
          rsync = "${pkgs.rsync}/bin/rsync";
          sqlite3 = if importHostStoreDb then "${pkgs.sqlite.bin}/bin/sqlite3" else null;
          bash = "${pkgs.bash}/bin/bash";
          # The agent-mode `start` recipe tail-calls `prompt` in a child shell
          # that need not have `katsuctl` on PATH, so it references this absolute
          # path. Same binary the menu commands / apps.sandbox invoke.
          katsuctl = "${katsuctlPkg}/bin/katsuctl";
        };
        runner = "${runner}/bin/microvm-run";
        diskImages = [
          "rw-store.img"
          "nix-db.img"
          "scratch.img"
        ];
        context = validatedContext;
        secrets = lib.mapAttrsToList (name: spec: {
          inherit name;
          source =
            if spec ? fromEnv then
              { fromEnv = spec.fromEnv; }
            else if spec ? fromFile then
              { fromFile = spec.fromFile; }
            else
              throw "katsuobushi.lib.sandbox: secret ${name} needs fromEnv or fromFile.";
          dest = "cred-${name}";
        }) secrets;
        vsockPort = 1024;
        hostCid = 2;
      }
      // lib.optionalAttrs graphicsCfg.enable {
        # The spec graphics block (camelCase: enable, gpu[], output{width,height,
        # refresh}), emitted only when enabled (so a graphics-off spec is
        # unchanged apart from the already-bumped specVersion). The host-side
        # GPU resolver (start.rs) consumes `gpu`; the guest display stack
        # consumes `output`.
        graphics = {
          enable = true;
          inherit (graphicsCfg) gpu;
          output = {
            inherit (graphicsCfg.output) width height refresh;
          };
        };
      }
    )
  );

  # Every command invokes `katsuctl` by its absolute store path
  # (`${katsuctlPkg}/bin/katsuctl`) rather than a bare name, so the commands work
  # for any consumer who wires `menuCommands` into a devshell without separately
  # putting `katsuctl` on PATH. The emitted agent-start recipe's `prompt`
  # tail-call self-references the same binary via `spec.tools.katsuctl`, so no
  # command needs to mutate PATH.
  #
  # Business logic lives in katsuctl; these wrappers only dispatch, in one of
  # two documented forms:
  #
  # - `handOff`: run the subcommand, passing the Nix-rendered spec via --config
  #   (prompt/status/fetch/stop/screenshot).
  # - `emitExec`: katsuctl makes every probe-dependent decision and prints only
  #   the path of a flat recipe; the wrapper `exec`s bash on it. A planning
  #   failure exits nonzero with no path, so the `exec` is reached only on a
  #   clean emit (start/attach — also `apps.sandbox` below).
  #
  # Usage-line rewrite: clap qualifies its usage/error text with the real binary
  # path — e.g. `Usage: katsuctl sandbox --config <CONFIG> attach <INSTANCE>` —
  # which is confusing when the user typed `sandbox attach`. Both wrappers pass
  # katsuctl's STDERR (where clap prints errors and `Usage:` lines) through
  # `usageSed`, which rewrites that prefix back to `sandbox `. Only stderr is
  # filtered, so a subcommand's real stdout — notably `sandbox prompt`'s live
  # report stream — flows straight through, unbuffered. (A `--help` invocation,
  # which clap prints to stdout, is not rewritten for that reason.)
  katsuctlBin = "${katsuctlPkg}/bin/katsuctl";
  usageSed = "${pkgs.gnused}/bin/sed -E 's|katsuctl sandbox --config <CONFIG> |sandbox |g'";

  # handOff runs katsuctl in the foreground (not `exec`, so the filter shell
  # survives to post-process stderr). stdout is routed to fd3 → the real stdout,
  # untouched; stderr goes through usageSed. `|| ret=$?` keeps errexit from
  # exiting before we capture katsuctl's status (pipefail makes the pipeline
  # yield it), which is then re-raised.
  handOff = subcommand: description: {
    inherit description;
    command = ''
      ret=0
      { ${katsuctlBin} sandbox --config ${katsuctlSpec} ${subcommand} "$@" 2>&1 1>&3 | ${usageSed} >&2; } 3>&1 || ret=$?
      exit "$ret"
    '';
  };
  # emitExec captures katsuctl's stdout (the recipe path), so its stderr is
  # buffered to a temp file and rewritten after the run — deterministic, with no
  # process-substitution flush race. On success the path is exec'd; on failure
  # the (rewritten) usage/error has already been shown and we exit nonzero.
  emitExecScript = subcommand: ''
    err="$(${pkgs.coreutils}/bin/mktemp)"
    ret=0
    script="$(${katsuctlBin} sandbox --config ${katsuctlSpec} ${subcommand} "$@" 2>"$err")" || ret=$?
    ${usageSed} <"$err" >&2
    ${pkgs.coreutils}/bin/rm -f "$err"
    [ "$ret" -eq 0 ] || exit "$ret"
    exec ${pkgs.bash}/bin/bash "$script"
  '';
  emitExec = subcommand: description: {
    inherit description;
    command = emitExecScript subcommand;
  };

  # One `sandbox` branch; the lifecycle verbs are its subcommands (`sandbox
  # start`, `sandbox prompt`, …). Each leaf keeps the same handOff/emitExec body
  # as before — the branch only groups them into a single binary + menu row.
  menuCommands = {
    sandbox = {
      description = "Launch and drive ephemeral agent sandbox VMs";
      help = "Manage Katsuobushi agent sandbox VMs. Run `sandbox <subcommand> --help` for a subcommand's own options.";
      subcommands = {
        # start: naming, port/CID allocation, branch seed, instance.json, then a
        # flat setup+boot recipe.
        start = emitExec "start" "Launch an agent sandbox";
        # dispatch: reads a project-board card, guards Available-only, claims it to
        # in-progress, and emit-execs the same agent-start recipe as `start` —
        # seeded with the card body as the directive. `--board-dir` defaults to
        # `project/kanban`; pass it for a non-default board.
        dispatch = emitExec "dispatch" "Launch an agent VM to work a project-board card";
        # prompt: instance resolution, the QMP liveness probe, the readiness-wait,
        # vsock streaming, and the paused-named auto-restart.
        prompt = handOff "prompt" "Send a prompt to an agent instance (auto-starting a paused one)";
        # status: a bare `status` doubles as the launch prerequisite gate (nonzero
        # exit iff a secret is missing or /dev/vhost-vsock is absent).
        status = handOff "status" "List instances, or detail a single instance";
        fetch = handOff "fetch" "Fetch a sandbox's branch into this repo";
        stop = handOff "stop" "Suspend or remove an instance";
        # attach: the running/has-session probes, then a tiny terminal-handoff
        # recipe.
        attach = emitExec "attach" "SSH in and attach to a running agent's tmux session";
        # screenshot: streams `grim -` over the loopback ssh as the agent user,
        # landing the PNG at the requested path (or host stdout for `-`). Requires
        # graphics.enable; katsuctl fails clearly if off.
        screenshot = handOff "screenshot" "Grab the headless compositor framebuffer of a graphics instance";
      };
    };
  };
in
{
  # `nix run .#sandbox` needs an app; lifecycle helpers are menu commands. Its
  # program is the SAME emit+exec wrapper `sandbox start` uses (one definition,
  # not a third copy). katsuctl is invoked by its absolute store path, and the
  # emitted agent-start tail-call self-references the same binary via
  # `spec.tools.katsuctl` — so nothing here touches PATH.
  apps.sandbox = {
    type = "app";
    program = "${pkgs.writeShellScript "sandbox" (emitExecScript "start")}";
    meta.description = "Launch an ephemeral Katsuobushi agent sandbox VM";
  };

  inherit menuCommands;

  # The host-side controller binary (`katsuctl`), built reproducibly via
  # lib.rust/crane from Katsuobushi's pinned source. The `sandbox:*` commands
  # already invoke it by absolute store path, so consumers need not add it for
  # those to work; it is exposed so power-users can put a bare `katsuctl` on their
  # devshell PATH (and so the dogfood flake reuses one build instead of two).
  katsuctl = katsuctlPkg;

  # The Nix-rendered instance spec, exposed so the menu-command rewrites
  # (step 3) can pass it to `katsuctl --config`.
  inherit katsuctlSpec;

  # Building the guest image so CI catches a broken sandbox config.
  checks.sandbox = runner;

  # The assembled guest system, exposed for advanced/inspection use.
  nixosConfiguration = guestSystem;
}
