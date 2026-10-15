{
  inputs = {
    v_flakes.url = "github:valeratrades/v_flakes?ref=v1.6";
  };
  outputs = { self, v_flakes }:
    let
      inherit (v_flakes) flake-utils pre-commit-hooks rust-overlay;
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import v_flakes.default_nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
          config.allowUnfree = true;
        };
        # Canonical toolchain pinned in v_flakes — byte-identical across repos (banking
        # builds the code invest-core was carved from with the same one), so the nix store
        # dedups it.
        rust = v_flakes.rs.default_nightly system;
        # Lean toolchain for the release image: rustc + cargo + std only.
        rustBuild = v_flakes.rs.build_nightly system;
        pre-commit-check = pre-commit-hooks.lib.${system}.run (v_flakes.files.preCommit { inherit pkgs; });
        pname = "ev_invest";

        # ── dev topology ────────────────────────────────────────────────────
        # Postgres is the SHARED ev_invest cluster every sibling repo uses (banking owns its
        # bring-up; this repo only ensures its own database, `invest`). TigerBeetle is
        # invest's OWN cluster (D19, plan §6.2) — 3034, beside banking's 3033, so both run
        # at once. invest-core's gRPC takes 50071, a 5007x range of its own (banking holds
        # 5005x/5006x, admin 50066). DEFAULTS, not overrides: anything already in the
        # environment (or a sourced `.env`) wins.
        corePort = 50071;
        ports = {
          POSTGRES_PORT = "5432";
          TIGERBEETLE_PORT = "3034";
          INVEST_CORE_PORT = toString corePort;
          CONCIERGE_PORT = "55670";
          SCOPE_LEDGER_PORT = "50054";
        };
        portEnv = pkgs.lib.concatStrings (pkgs.lib.mapAttrsToList (n: v: "export ${n}=\"\${${n}:-${v}}\"\n") ports);

        rs = v_flakes.rs { inherit pkgs rust; };
        github = v_flakes.github {
          inherit pkgs pname rs;
          enable = true;
          lastSupportedVersion = "nightly-2026-05-12";
          containerRelease = { registry = "ghcr.io/ev-fund"; };
          gitignore.extra = ''
            ## Local TigerBeetle
            .tb/
            .tb-client
            ## Env
            .env
            .env.local
            ## Nix build outputs beyond the base `result`
            result-*
          '';
          jobs = {
            warnings.augment = [ "tokei" "code-duplication" ];
          };
        };
        # No readme-fw: it regenerates README.md on every shell entry, and the README here
        # is hand-written.
        combined = v_flakes.utils.combine { inherit rust; modules = [ rs github ]; };

        # tonic-build / prost-build shell out to protoc; point them at nixpkgs'.
        protocEnv = ''export PROTOC="${pkgs.protobuf}/bin/protoc"'';
        # rust-lld embeds the wrong rpath on macOS; the FALLBACK var only kicks in when
        # normal resolution fails (see banking's flake for the full story).
        dyldFallback = ''export DYLD_FALLBACK_LIBRARY_PATH="${rust}/lib''${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"'';
        # The generated `.cargo/config.toml` routes rustc through sccache. Not here: its
        # cache costs GBs at a near-zero hit rate on this machine. An empty RUSTC_WRAPPER is
        # cargo's documented off switch and wins over the config file.
        noWrapper = ''export RUSTC_WRAPPER=""'';

        # ── TigerBeetle (server binary + Rust client) — the same 0.17.6 banking runs ──
        # Upstream's precompiled zig instead of nixpkgs zig, whose dynamic libLLVM turns
        # the one-time client build into a multi-GB download.
        zigBin =
          let
            dist = {
              x86_64-linux = { suffix = "x86_64-linux"; sha256 = "24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c"; };
              aarch64-linux = { suffix = "aarch64-linux"; sha256 = "f7a654acc967864f7a050ddacfaa778c7504a0eca8d2b678839c21eea47c992b"; };
              x86_64-darwin = { suffix = "x86_64-macos"; sha256 = "b0f8bdfb9035783db58dd6c19d7dea89892acc3814421853e5752fe4573e5f43"; };
              aarch64-darwin = { suffix = "aarch64-macos"; sha256 = "39f3dc5e79c22088ce878edc821dedb4ca5a1cd9f5ef915e9b3cc3053e8faefa"; };
            }.${system};
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "zig-bin";
            version = "0.14.1";
            src = pkgs.fetchurl {
              url = "https://ziglang.org/download/0.14.1/zig-${dist.suffix}-0.14.1.tar.xz";
              inherit (dist) sha256;
            };
            dontConfigure = true;
            dontBuild = true;
            dontFixup = true;
            installPhase = ''
              mkdir -p $out/bin
              cp zig $out/bin/
              cp -r lib $out/lib
            '';
          };

        # Official precompiled server binary: a cluster evicts any client released after
        # it (client_release_too_high), so the server must be >= the client built below.
        tigerbeetleBin =
          let
            dist = {
              x86_64-linux = { file = "tigerbeetle-x86_64-linux.zip"; hash = "sha256-butV+rwsBnpLCCOV9KNzvCNCC8QbG/AR7ZRnl+Uyl7Y="; };
              aarch64-linux = { file = "tigerbeetle-aarch64-linux.zip"; hash = "sha256-JmsczIvW67WTrK0iCEDHcu9lhMyK84ZvhIs+lgL2bAs="; };
              x86_64-darwin = { file = "tigerbeetle-universal-macos.zip"; hash = "sha256-83nhQqHYu6PPKu4rH6rjD/J3hJinhXQ6b7C4hZ9//v8="; };
              aarch64-darwin = { file = "tigerbeetle-universal-macos.zip"; hash = "sha256-83nhQqHYu6PPKu4rH6rjD/J3hJinhXQ6b7C4hZ9//v8="; };
            }.${system};
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "tigerbeetle-bin";
            version = "0.17.6";
            src = pkgs.fetchurl {
              url = "https://github.com/tigerbeetle/tigerbeetle/releases/download/0.17.6/${dist.file}";
              inherit (dist) hash;
            };
            nativeBuildInputs = [ pkgs.unzip ];
            unpackPhase = "unzip $src";
            dontConfigure = true;
            dontBuild = true;
            dontFixup = true;
            installPhase = ''
              mkdir -p $out/bin
              install -m755 tigerbeetle $out/bin/
            '';
          };

        # The native C client library + header, so the official tigerbeetle Rust crate
        # can link against them; the output is src/clients/rust/ ready as a path dep.
        tigerbeetleClient = pkgs.stdenv.mkDerivation {
          name = "tigerbeetle-client";
          src = pkgs.fetchzip {
            url = "https://github.com/tigerbeetle/tigerbeetle/archive/refs/tags/0.17.6.tar.gz";
            hash = "sha256-b519nsDbas+XOw3ulAnzpk2KwtJkeOC3e13urM2tUSM=";
          };
          # TigerBeetle pins its zig version exactly (0.17.6 → zig 0.14.1).
          nativeBuildInputs = [ zigBin pkgs.git ];
          # build.zig runs `git tag --merged HEAD^` at configure time and consumes 4+
          # version-shaped tags; the release tarball has no .git, so fabricate a history.
          postPatch = ''
            git init -q
            git -c user.name=nix -c user.email=nix@localhost add -A
            git -c user.name=nix -c user.email=nix@localhost commit -qm base
            for v in 0.17.1 0.17.2 0.17.3 0.17.4 0.17.5; do git tag "$v"; done
            git -c user.name=nix -c user.email=nix@localhost commit -qm head --allow-empty
          '';
          buildPhase = ''
            export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
            zig build clients:rust -Drelease \
              -Dgit-commit=64899c7a41fd3d74c68da7bb2efcb7d208abd5f2 \
              -Dconfig-release=0.17.6 -Dconfig-release-client-min=0.17.6
          '';
          installPhase = ''
            mkdir -p $out
            cp -r src/clients/rust/* $out/
            printf '\n[workspace]\n' >> $out/Cargo.toml
          '';
        };

        # Symlink the TigerBeetle Rust client so the workspace's path dependency resolves.
        # At the repo root, NOT under a member dir: cargo's workspace exclude can never
        # match a path inside a member's directory.
        linkTbClient = ''
          tb_client_dir="$(git rev-parse --show-toplevel)/.tb-client"
          if [ ! -L "$tb_client_dir" ] || [ "$(readlink "$tb_client_dir")" != "${tigerbeetleClient}" ]; then
            rm -rf "$tb_client_dir"
            ln -s "${tigerbeetleClient}" "$tb_client_dir"
          fi
        '';

        # ── production binary + OCI image ───────────────────────────────────
        coreCargo = (builtins.fromTOML (builtins.readFile ./core/Cargo.toml)).package;
        rustPlatform = pkgs.makeRustPlatform { cargo = rustBuild; rustc = rustBuild; };
        buildSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          # .cargo holds dev-only accelerators the hermetic sandbox lacks; .tb-client is
          # recreated in-sandbox below.
          filter = path: _type:
            ! builtins.elem (baseNameOf path) [ "target" ".tb-client" ".tb" ".direnv" ".git" ".cargo" "tmp" "docs" "result" ];
        };
        coreBin = rustPlatform.buildRustPackage {
          pname = coreCargo.name;
          version = coreCargo.version;
          src = buildSrc;
          cargoLock = {
            lockFile = ./Cargo.lock;
            # evbanking_contracts / evconcierge_auth are rev-pinned public git deps —
            # builtin fetchGit needs no hash.
            allowBuiltinFetchGit = true;
          };
          postPatch = "ln -sfn ${tigerbeetleClient} .tb-client";
          # Forget this flag and the image builds fine with no binary inside.
          cargoBuildFlags = [ "-p" "invest-core" ];
          nativeBuildInputs = with pkgs; [ protobuf pkg-config ];
          buildInputs = [ pkgs.openssl ];
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          doCheck = false;
        };

        # Secrets (SENTRY_DSN, SCOPE_LEDGER_SERVICE_TOKEN, the mTLS key) arrive via the k8s
        # Secret gitops owns — never baked here. `--print-required-vars` on the image is the
        # contract the gitops preflight diffs that Secret against.
        containerStd = v_flakes.container.implement {
          inherit pkgs pname;
          containers.invest-core = {
            port = corePort;
            # gRPC-only server: gitops swaps these httpGet probes for native `grpc` probes
            # (the standard grpc.health.v1 service answers them); the path is a required
            # contract placeholder.
            healthPath = "/";
            criticality = "normal";
            entrypoint = [ "/bin/invest-core" ];
            contents = [ coreBin ];
            env = {
              RUST_LOG = "info";
              # The canonical prod topology (secret-free, committable): the binary reads env
              # only (ev_lib::settings), and gitops' Deployment env carries the same values.
              # TIGERBEETLE_ADDRESS / TIGERBEETLE_CLUSTER_ID are deliberately absent: invest's
              # own cluster (plan §11) has no address yet, and a guessed one here would
              # override what gitops sets. The boot refuses to start without them.
              DATABASE_URL = "postgres://evinvest@10.42.0.1:5432/invest";
              GRPC_ADDR = "0.0.0.0:${toString corePort}";
              # Access tokens of concierge's `invest` relying party.
              AUTH_ISSUER = "https://auth.concierge.ev";
              AUTH_AUDIENCE = "invest";
              AUTH_JWKS_GRPC_ENDPOINT = "http://concierge.ev-banking.svc.cluster.local:55670";
              APP_ENV = "production";
            };
          };
        };

        # ── shared Postgres (ensure-running) ────────────────────────────────
        # The SAME cluster banking's `.#db` starts (state dir, port, trust auth); this one
        # only ensures invest's database exists. Stop: pg_ctl -D ~/.local/state/ev_invest/pg/data stop
        runPostgres = pkgs.writeShellApplication {
          name = "run-postgres";
          runtimeInputs = with pkgs; [ postgresql coreutils gnugrep util-linux ];
          text = ''
            ${portEnv}
            state="''${XDG_STATE_HOME:-$HOME/.local/state}/ev_invest"
            export PGDATA="$state/pg/data"
            sockets="$state/pg/sockets"
            dbs="''${PGDATABASES:-invest}"
            mkdir -p "$sockets"

            # Serialize sibling repos racing to first-boot the shared cluster.
            exec 9>"$state/pg.lock"
            flock 9

            if ! pg_isready --host="$sockets" --port="$POSTGRES_PORT" --quiet; then
              # TCP answering while our socket is silent = some OTHER cluster owns the
              # port — refuse rather than silently use the wrong database.
              if pg_isready --host=127.0.0.1 --port="$POSTGRES_PORT" --quiet; then
                echo "error: 127.0.0.1:$POSTGRES_PORT serves a postgres that is not the shared ev_invest cluster" >&2
                exit 1
              fi
              if [ ! -s "$PGDATA/PG_VERSION" ]; then
                echo "initialising shared postgres cluster in $PGDATA"
                initdb --username=postgres --auth=trust --pgdata="$PGDATA" >/dev/null
              fi
              chmod 0700 "$PGDATA"
              # 9>&- : the daemon must NOT inherit the lock fd, or it holds the flock for
              # its lifetime and every later ensure-run blocks forever.
              pg_ctl -D "$PGDATA" -l "$state/pg/log" -o "-k $sockets -h 127.0.0.1 -p $POSTGRES_PORT" start 9>&-
            fi
            exec 9>&-

            for db in $dbs; do
              if ! psql --host="$sockets" --port="$POSTGRES_PORT" --username=postgres --dbname=postgres \
                     --tuples-only --no-align \
                     --command "SELECT 1 FROM pg_database WHERE datname='$db'" | grep -q 1; then
                createdb --host="$sockets" --port="$POSTGRES_PORT" --username=postgres "$db"
                echo "created database '$db'"
              fi
            done
            echo "postgres ready on 127.0.0.1:$POSTGRES_PORT (databases ensured: $dbs)"
          '';
        };

        # ── local TigerBeetle ───────────────────────────────────────────────
        # invest's own ledger under .tb/ (gitignored). First run formats a single-replica
        # cluster; later runs just start it.
        runTigerbeetle = pkgs.writeShellApplication {
          name = "run-tigerbeetle";
          runtimeInputs = [ tigerbeetleBin pkgs.git ];
          text = ''
            ${portEnv}
            repo="$(git rev-parse --show-toplevel)"
            # Overridable so a gate can point a throwaway replica away from the dev ledger.
            export TB_DATA="''${TB_DATA:-$repo/.tb/data}"
            port="$TIGERBEETLE_PORT"
            cluster_id="''${TBCLUSTER:-0}"
            data_file="$TB_DATA/''${cluster_id}_0.tigerbeetle"

            mkdir -p "$TB_DATA"
            if [ ! -f "$data_file" ]; then
              echo "formatting TigerBeetle data file (cluster=''${cluster_id}, replica=0, replica-count=1)"
              tigerbeetle format --cluster="$cluster_id" --replica=0 --replica-count=1 "$data_file"
            fi

            echo "TigerBeetle ready on 127.0.0.1:$port (cluster ''${cluster_id})"
            exec tigerbeetle start --addresses="127.0.0.1:$port" "$data_file"
          '';
        };

        # ── invest-core ─────────────────────────────────────────────────────
        # Applies its migrations on boot; needs Postgres + TigerBeetle (`.#db` + `.#tb`, or
        # `.#dev`). Defaults mirror core/.env.example; any value already set wins.
        runCore = pkgs.writeShellApplication {
          name = "run-invest-core";
          runtimeInputs = with pkgs; [ rust pkg-config openssl protobuf mold git ];
          text = ''
            ${dyldFallback}
            ${protocEnv}
            ${noWrapper}
            repo="$(git rev-parse --show-toplevel)"
            cd "$repo"
            ${linkTbClient}

            set -a
            if [ -f core/.env ]; then
              # shellcheck disable=SC1091
              . core/.env
            fi
            set +a

            ${portEnv}
            export DATABASE_URL="''${DATABASE_URL:-postgres://postgres@localhost:$POSTGRES_PORT/invest}"
            export GRPC_ADDR="''${GRPC_ADDR:-127.0.0.1:$INVEST_CORE_PORT}"
            export TIGERBEETLE_ADDRESS="''${TIGERBEETLE_ADDRESS:-127.0.0.1:$TIGERBEETLE_PORT}"
            export TIGERBEETLE_CLUSTER_ID="''${TIGERBEETLE_CLUSTER_ID:-0}"
            export AUTH_JWKS_GRPC_ENDPOINT="''${AUTH_JWKS_GRPC_ENDPOINT:-http://127.0.0.1:$CONCIERGE_PORT}"
            export APP_ENV="''${APP_ENV:-development}"
            export RUST_LOG="''${RUST_LOG:-info,invest_core=debug}"
            exec cargo run -p invest-core
          '';
        };

        # ── dev orchestrator ────────────────────────────────────────────────
        # Ensures the SHARED postgres (detached, survives this stack), then owns TigerBeetle
        # + invest-core; one trap tears the owned tree down. Concierge and banking run from
        # their own repos.
        runDev = pkgs.writeShellApplication {
          name = "run-dev";
          runtimeInputs = with pkgs; [ coreutils ];
          text = ''
            ${portEnv}
            pids=()
            cleanup() {
              echo; echo "shutting down dev stack…"
              [ ''${#pids[@]} -gt 0 ] && kill "''${pids[@]}" 2>/dev/null || true
              wait 2>/dev/null || true
            }
            trap cleanup EXIT INT TERM

            echo "▶ postgres (shared)"
            ${runPostgres}/bin/run-postgres
            echo "▶ tigerbeetle (:$TIGERBEETLE_PORT)"
            ${runTigerbeetle}/bin/run-tigerbeetle & pids+=($!)
            echo "▶ invest-core (:$INVEST_CORE_PORT)"
            ${runCore}/bin/run-invest-core & pids+=($!)
            wait
          '';
        };

        # ── the build/test gate (CI entry point) ────────────────────────────
        # clippy with warnings denied, then the workspace tests against a THROWAWAY Postgres
        # and TigerBeetle on ports away from the dev ones (and from banking's gate, 54329 /
        # 3039), with CI=true so tests/common FAILS instead of skipping when a service is
        # missing — a green run that touched neither is the outcome this gate exists to stop.
        # /tmp rather than mktemp's default: macOS's $TMPDIR pushes the postgres socket path
        # past the 103-byte sun_path limit.
        runCheckRust = pkgs.writeShellApplication {
          name = "run-check-rust";
          # mold: the generated .cargo/config.toml links x86_64-linux with `-fuse-ld=mold`,
          # and a CI runner has no linker of that name outside the dev shell (banking's gate
          # carries it for the same reason).
          # No pkg-config/openssl here, as in banking: nixpkgs' pkg-config searches only the
          # store, so openssl-sys (native-tls under ev_lib's reqwest and sentry) would build
          # against nixpkgs' openssl and load its glibc at test time beside the runner's
          # (GLIBC_ABI_DT_X86_64_PLT not found). The runner's own pkg-config finds its libssl.
          runtimeInputs = (with pkgs; [ rust git protobuf mold postgresql coreutils ]) ++ [ tigerbeetleBin ];
          text = ''
            cd "$(git rev-parse --show-toplevel)"
            ${linkTbClient}
            ${protocEnv}
            ${noWrapper}

            echo "▶ clippy (workspace, all targets, warnings denied)"
            cargo clippy --workspace --all-targets -- -D warnings

            scratch="$(mktemp -d /tmp/invest-check.XXXXXX)"
            export XDG_STATE_HOME="$scratch/state"
            export TB_DATA="$scratch/tb"
            export POSTGRES_PORT="''${CHECK_POSTGRES_PORT:-54339}"
            export TIGERBEETLE_PORT="''${CHECK_TIGERBEETLE_PORT:-3049}"
            export PGDATABASES="invest_check"
            tb_pid=""
            cleanup() {
              if [ -n "$tb_pid" ]; then kill "$tb_pid" 2>/dev/null || true; fi
              pg_ctl -D "$XDG_STATE_HOME/ev_invest/pg/data" -m fast stop >/dev/null 2>&1 || true
              rm -rf "$scratch"
            }
            trap cleanup EXIT

            echo "▶ postgres :$POSTGRES_PORT + tigerbeetle :$TIGERBEETLE_PORT (throwaway)"
            ${runPostgres}/bin/run-postgres
            TBCLUSTER=0 ${runTigerbeetle}/bin/run-tigerbeetle >"$scratch/tb.log" 2>&1 &
            tb_pid=$!
            for _ in $(seq 1 150); do
              if (exec 3<>"/dev/tcp/127.0.0.1/$TIGERBEETLE_PORT") 2>/dev/null; then break; fi
              if ! kill -0 "$tb_pid" 2>/dev/null; then cat "$scratch/tb.log" >&2; echo "tigerbeetle exited" >&2; exit 1; fi
              sleep 0.2
            done
            if ! (exec 3<>"/dev/tcp/127.0.0.1/$TIGERBEETLE_PORT") 2>/dev/null; then
              cat "$scratch/tb.log" >&2; echo "tigerbeetle did not come up on :$TIGERBEETLE_PORT" >&2; exit 1
            fi
            export DATABASE_URL="postgres://postgres@127.0.0.1:$POSTGRES_PORT/invest_check"
            export TIGERBEETLE_ADDRESS="127.0.0.1:$TIGERBEETLE_PORT"
            export TIGERBEETLE_CLUSTER_ID=0
            export CI=true

            echo "▶ tests (workspace, integration suites against the throwaway services)"
            cargo test --workspace

            echo "✓ rust ok"
          '';
        };

        # `nix run .#drift-check` — Cargo.lock agrees with the manifests. `--locked` refuses
        # to re-resolve, so a merge that took a new git rev in Cargo.toml while keeping the
        # old one in Cargo.lock fails HERE rather than at the image build.
        runDriftCheck = pkgs.writeShellApplication {
          name = "run-drift-check";
          runtimeInputs = [ rust pkgs.git ];
          text = ''
            cd "$(git rev-parse --show-toplevel)"
            ${linkTbClient}
            ${noWrapper}
            cargo metadata --locked --format-version 1 >/dev/null
            echo "✓ no drift"
          '';
        };

        runHelp = pkgs.writeShellScriptBin "help" ''
          cat <<EOF
          nix run .#dev           postgres (shared) + invest's tigerbeetle (:3034) + invest-core (:${toString corePort})
          nix run .#invest-core   invest-core alone (needs .#db and .#tb)
          nix run .#db            ensure the shared Postgres and the 'invest' database
          nix run .#tb            invest's local TigerBeetle on :3034
          nix run .#check-rust    clippy (warnings denied) + workspace tests, as CI runs them
          nix run .#drift-check   Cargo.lock matches the manifests
          nix build .#ev_invest-invest-core-container   the invest-core OCI image (containers.invest-core)
          EOF
        '';
      in
      {
        # `nix run .#dev`         → postgres (shared) + tigerbeetle + invest-core
        # `nix run .#invest-core` → invest-core alone (applies migrations on boot; needs `.#db` + `.#tb`)
        # `nix run .#db`          → ensure the SHARED ev_invest Postgres is up (+ the `invest` database)
        # `nix run .#tb`          → invest's local TigerBeetle on :3034 (repo-local data under .tb/)
        # `nix run .#check-rust`  → clippy (warnings denied) + workspace tests, as CI runs them
        # `nix run .#drift-check` → Cargo.lock matches the manifests
        # Author new migrations with the sqlx CLI (in the dev shell):
        #   sqlx migrate add --source core/migrations --sequential <name>
        apps = {
          dev = { type = "app"; program = "${runDev}/bin/run-dev"; };
          invest-core = { type = "app"; program = "${runCore}/bin/run-invest-core"; };
          db = { type = "app"; program = "${runPostgres}/bin/run-postgres"; };
          tb = { type = "app"; program = "${runTigerbeetle}/bin/run-tigerbeetle"; };
          check-rust = { type = "app"; program = "${runCheckRust}/bin/run-check-rust"; };
          drift-check = { type = "app"; program = "${runDriftCheck}/bin/run-drift-check"; };
          help = { type = "app"; program = "${runHelp}/bin/help"; };
        };

        packages = {
          default = coreBin;
        } // containerStd.packages;

        containers = containerStd.containers;

        devShells.default =
          with pkgs;
          mkShell {
            shellHook =
              pre-commit-check.shellHook
              + combined.shellHook
              + ''
                cp -f ${(v_flakes.files.treefmt) { inherit pkgs; }} ./.treefmt.toml
                ${portEnv}
                ${dyldFallback}
                ${protocEnv}
                ${noWrapper}
                ${linkTbClient}
              '';

            packages = [
              openssl
              pkg-config
              protobuf
              rust
              mold
              postgresql
              sqlx-cli
              tigerbeetleBin
            ] ++ pre-commit-check.enabledPackages ++ combined.enabledPackages;

            env.RUST_BACKTRACE = 1;
            env.RUST_LIB_BACKTRACE = 0;
          };
      }
    );
}
