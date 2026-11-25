#!/bin/sh
# Initial transfer and delegation only. Project operations run in the native engine.
# devkit-stand-protocol-v1
set -eu
mode=@@DEVKIT_MODE@@
version=@@DEVKIT_VERSION@@
fail() { printf 'devkit: %s\n' "$*" >&2; exit 1; }
host_os=$(uname -s)
host_cpu=$(uname -m)
case "$host_os:$host_cpu" in
# @@DEVKIT_ENGINE_PINS@@
  *) fail "this release does not support $host_os $host_cpu" ;;
esac
expected=$sha256
asset=devkit-$target
command=${1:-help}
root=
loader_directory=${0%/*}
[ "$loader_directory" != "$0" ] || loader_directory=.
if [ "$mode" = project ]; then
  root=$(CDPATH= cd -- "$loader_directory/../.." && pwd -P)
fi
if [ "$mode" = project ] && [ "$command" = worktree-hook ]; then
  [ "$#" -eq 2 ] || fail 'worktree-hook requires reserve, start, or remove'
  unset MAKEFLAGS MFLAGS MAKEOVERRIDES GNUMAKEFLAGS MAKEFILES MAKELEVEL MAKECMDGOALS
  unset app package component follow tail headless cleanup reconcile preview json
  unset name template api use profile to command cmd write network prepare toolchain
  unset DEVKIT_APP DEVKIT_PACKAGE DEVKIT_TARGET_COMPONENT DEVKIT_FOLLOW DEVKIT_TAIL
  unset DEVKIT_HEADLESS DEVKIT_CLEANUP DEVKIT_PACKAGE_AGGREGATE DEVKIT_DEV_GROUP
  unset DEVKIT_SYNC_RECONCILE DEVKIT_SYNC_PREVIEW DEVKIT_INFO_JSON DEVKIT_EXPECTED_MANIFEST
  unset DEVKIT_EXPECTED_PLAN DEVKIT_MAKE_PLAN DEVKIT_MAKE_DAG DEVKIT_ENGINE_PATH DEVKIT_LINUX_HELPER_PATH
  unset DEVKIT_LIFECYCLE_NAME DEVKIT_LIFECYCLE_TEMPLATE DEVKIT_LIFECYCLE_API
  unset DEVKIT_LIFECYCLE_USE DEVKIT_LIFECYCLE_PROFILE DEVKIT_LIFECYCLE_TO
  unset DEVKIT_EXEC_COMMAND DEVKIT_EXEC_COMPONENT DEVKIT_EXEC_CONFLICT
  unset DEVKIT_EXEC_WRITE DEVKIT_EXEC_NETWORK DEVKIT_EXEC_PREPARE DEVKIT_EXEC_TOOLCHAIN
  cd -- "$root"
  case "$2" in
    reserve)
      if ! sh .devkit/runtime/devkit.sh cli --non-interactive stand reserve .; then
        printf '%s\n' 'devkit: address reservation skipped; run make up when ready' >&2
      fi ;;
    start)
      [ "${DEVKIT_WT_AUTOSTART:-0}" != 1 ] || { make setup && make up; } ;;
    remove)
      exec sh .devkit/runtime/devkit.sh cli --non-interactive stand remove . ;;
    *) fail 'worktree-hook requires reserve, start, or remove' ;;
  esac
  exit $?
fi
if [ "$mode" = project ] && [ "$command" = down ]; then
  case "${DEVKIT_CLEANUP:-0}" in 0|1) ;; *) fail 'cleanup must be 0 or 1' ;; esac
  bounded_git() {
    if [ "$host_os" = Darwin ]; then
      # Isolate stdout so a slow descendant cannot keep the loader's pipe open.
      /usr/bin/env -i PATH=/usr/bin:/bin /usr/bin/perl -MPOSIX=setpgid -MTime::HiRes=alarm -e '
        my $pid = open(my $output, "-|");
        defined($pid) or exit 1;
        if ($pid == 0) {
          setpgid(0, 0) == 0 or exit 1;
          exec @ARGV;
          exit 1;
        }
        my ($path, $success);
        local $SIG{ALRM} = sub { die "deadline" };
        my $ok = eval {
          alarm(0.25);
          local $/;
          $path = <$output>;
          $success = close($output);
          alarm(0);
          1;
        };
        unless ($ok) {
          alarm(0);
          kill "KILL", -$pid;
          kill "KILL", $pid;
          waitpid($pid, 0);
          exit 1;
        }
        exit 1 unless $success;
        print $path;
      ' -- "$@"
    else
      timeout --kill-after=0.05s 0.25s "$@"
    fi
  }
  git_history_absent() {
    [ -e "$root/.git" ] || [ -L "$root/.git" ] || return 0
    [ ! -L "$root/.git" ] || return 1
    admin=$(bounded_git env -i PATH="$PATH" LC_ALL=C GIT_CONFIG_NOSYSTEM=1 \
      GIT_CONFIG_GLOBAL=/dev/null GIT_OPTIONAL_LOCKS=0 \
      git -C "$root" -c core.fsmonitor=false rev-parse --absolute-git-dir 2>/dev/null) || return 1
    [ -d "$admin" ] && [ -x "$admin" ] && [ -r "$admin" ] \
      && [ ! -e "$admin/devkit-stand-witness.json" ] && [ ! -L "$admin/devkit-stand-witness.json" ]
  }
  if [ -z "${DEVKIT_APP:-}${DEVKIT_PACKAGE:-}" ] \
    && [ -d "$root/.devkit" ] && [ -r "$root/.devkit" ] && [ -x "$root/.devkit" ] && [ ! -L "$root/.devkit" ] \
    && [ ! -e "$root/.devkit/update-pending.json" ] && [ ! -L "$root/.devkit/update-pending.json" ] \
    && [ ! -e "$root/.devkit/state" ] && [ ! -L "$root/.devkit/state" ] \
    && [ ! -e "$root/.devkit/cache" ] && [ ! -L "$root/.devkit/cache" ] && git_history_absent; then
    exit 0
  fi
fi

# Bound both the pathname checks and the read. Compare the open file with its
# pathname before and after, so replacement or an in-progress write fails closed.
darwin_file() {
  /usr/bin/env -i /usr/bin/perl -MFcntl=:DEFAULT,:mode -MErrno=ENOENT -MTime::HiRes=stat,lstat,alarm -MDigest::SHA -e '
    use strict;
    my ($op, $path, $limit, $expected, $require_755) = @ARGV;
    alarm($limit <= 4194304 ? 0.25 : 2);
    sub stamp {
      return "" unless @_ && S_ISREG($_[2]);
      # Preserve all time precision exposed by Time::HiRes.
      return sprintf "%s/%s:%s:%s:%o/%s", @_[3,0,1,7], $_[2] & 07777,
        unpack("H*", pack("dd", @_[9,10]));
    }
    sub resolve_no_symlinks {
      my ($raw) = @_;
      my @parts;
      my $aliased = 0;
      for my $name (split m{/}, $raw) {
        next if $name eq "" || $name eq ".";
        if ($name eq "..") {
          if ($aliased && @parts == 2) {
            @parts = ();
            $aliased = 0;
          } else {
            pop @parts;
          }
          next;
        }
        my $cur = join "/", "", @parts, $name;
        my @info = lstat($cur);
        return undef if !@info && !$!{ENOENT};
        if (@info && S_ISLNK($info[2])) {
          my $alias = lc $name;
          return undef if @parts || $alias !~ /\A(?:tmp|var|etc)\z/;
          my @system = lstat("/$alias");
          return undef unless @system && $system[0] == $info[0] && $system[1] == $info[1];
          my $dest = readlink($cur);
          return undef unless defined($dest) && ($dest eq "private/$alias" || $dest eq "/private/$alias");
          push @parts, "private";
          $aliased = 1;
          $name = $alias;
          @info = lstat("/private/$name");
          return undef unless @info && S_ISDIR($info[2]);
        }
        push @parts, $name;
      }
      return "/" . join("/", @parts);
    }
    sub verify_engine_file {
      my ($engine, $max_bytes, $want_sha, $need_755) = @_;
      for my $attempt (1..2) {
        my @initial = lstat($engine);
        return 2 if !@initial && $!{ENOENT};
        my $st = stamp(@initial);
        return 3 unless length($st) && $initial[7] <= $max_bytes && -x $engine;
        return 3 if $need_755 && (($initial[2] & 07777) != 0755);
        sysopen(my $fh, $engine, O_RDONLY | O_NONBLOCK | O_NOFOLLOW) or return 3;
        my $sha = Digest::SHA->new(256);
        my $ok = (stamp(stat($fh)) eq $st);
        my $count = 0;
        while ($ok) {
          my $n = sysread($fh, my $data, 65536);
          if (!defined($n)) { $ok = 0; last; }
          last if $n == 0;
          $count += $n;
          if ($count > $initial[7]) { $ok = 0; last; }
          $sha->add($data);
        }
        if ($ok && $count == $initial[7]
          && stamp(stat($fh)) eq $st && stamp(lstat($engine)) eq $st) {
          return ($sha->hexdigest eq $want_sha) ? 0 : 3;
        }
        if ($attempt == 1 && $initial[3] == 2) {
          my @now = lstat($engine);
          if (@now && S_ISREG($now[2]) && $now[3] == 1
            && $now[0] == $initial[0] && $now[1] == $initial[1]
            && $now[7] == $initial[7] && ($now[2] & 07777) == ($initial[2] & 07777)) {
            next;
          }
        }
        return 3;
      }
      return 3;
    }
    if ($op eq "resolve-verify") {
      my $parent = resolve_no_symlinks($path);
      exit 1 unless defined($parent);
      print $parent;
      exit 4 if $parent =~ m{\A/engines/[^/]+/[^/]+/[^/]+\z};
      exit verify_engine_file("$parent/devkit", $limit, $expected, $require_755);
    }
    if ($op eq "verify") {
      exit verify_engine_file($path, $limit, $expected, $require_755);
    }
    my @initial = lstat($path);
    my $stamp = stamp(@initial);
    exit 1 unless length($stamp) && $initial[7] <= $limit;
    sysopen(my $file, $path, O_RDONLY | O_NONBLOCK | O_NOFOLLOW) or exit 1;
    exit 1 unless stamp(stat($file)) eq $stamp;
    my $sha = Digest::SHA->new(256);
    my $count = 0;
    while (1) {
      my $n = sysread($file, my $data, 65536);
      defined($n) or exit 1;
      last if $n == 0;
      $count += $n;
      exit 1 if $count > $initial[7];
      if ($op eq "read") { print $data; } else { $sha->add($data); }
    }
    exit 1 unless $count == $initial[7] && stamp(stat($file)) eq $stamp
      && stamp(lstat($path)) eq $stamp;
    print $sha->hexdigest if $op eq "hash";
  ' -- "$@"
}
read_small() {
  if [ "$host_os" = Darwin ]; then
    darwin_file read "$1" 4194304
    return
  fi
  timeout --kill-after=0.05s 0.25s sh -c '
    set -eu
    p=$1
    [ -f "$p" ] && [ ! -L "$p" ] && [ -r "$p" ]
    stamp=$(stat -c "%d:%i:%s:%z" -- "$p")
    size=${stamp#*:*:}
    size=${size%%:*}
    [ "$size" -le 4194304 ]
    exec 3<"$p"
    fd=/proc/$$/fd/3
    [ "$(stat -Lc "%d:%i:%s:%z" -- "$fd")" = "$stamp" ]
    head -c 4194305 <&3
    [ "$(stat -Lc "%d:%i:%s:%z" -- "$fd")" = "$stamp" ]
    [ "$(stat -c "%d:%i:%s:%z" -- "$p")" = "$stamp" ]
  ' sh "$1"
}
if [ "$mode" = project ] && [ "$command" = make-manifest ]; then
  [ -d "$root/.devkit" ] && [ ! -L "$root/.devkit" ] || exit 1
  [ -f "$root/.devkit/project.mk" ] && [ ! -L "$root/.devkit/project.mk" ] && [ -r "$root/.devkit/project.mk" ] || exit 1
  if [ "$host_os" = Darwin ]; then
    darwin_file hash "$root/.devkit/manifest.json" 4194304 && printf '\n'
    exit $?
  fi
  # Preserve trailing newlines when capturing the checked bytes in the shell.
  manifest=$(read_small "$root/.devkit/manifest.json" && printf .) || exit 1
  observed=$(printf '%s' "${manifest%.}" | sha256sum)
  printf '%s\n' "${observed%% *}"
  exit 0
fi
if [ "$mode" = project ] && [ "$command" = help ]; then
  printf '%s\n' \
    'make setup [app=NAME | package=NAME]     Prepare pinned tools and dependencies' \
    'make info [app=NAME] [json=1]           Show checkout readiness, services and preview addresses' \
    'make dev | up [app=NAME] [headless=1]   Start services in the foreground or background' \
    'make down [cleanup=1]                  Stop services, optionally release ports, preserve data' \
    'make logs [app=NAME] [tail=100] [follow=1]  Read service logs' \
    'make doctor                            Check the host and checkout' \
    'make check | fix | test | build [app=NAME | package=NAME]  Check, format, test or build' \
    'make package [app=DESKTOP | package=NAME]  Package a desktop app or public library' \
    'make exec [app=NAME | package=NAME | component=PATH | toolchain=node|rust] command=ARGV' \
    '          [write=1] [network=bridge] [prepare=1]  Run in the workspace or a selected component' \
    'make add-app name=NAME template=ID [api=NAME] [use="NAME ..."] [reconcile=1]' \
    'make add-package name=NAME template=ID [use="NAME ..."] [reconcile=1]' \
    '          App templates: node-api, rust-api, react-web, electron-react' \
    '          Package templates: ts-library, react-ui, react-client, rust-library' \
    'make register app=NAME|package=NAME profile=ID [reconcile=1]  Register existing source' \
    'make sync [reconcile=1] [preview=1]       Reconcile project metadata and generated files' \
    'make update to=VERSION [preview=1] [reconcile=1]  Upgrade the pinned Devkit release' \
    'make recover                            Roll back an interrupted project update' \
    '' \
    'Verification uses pinned offline containers. Run make setup to prepare them.' \
    'Use make exec for dependency changes, then make sync and make setup as needed.' \
    'Development ports are leased to this checkout and survive ordinary make down.'
  if [ -d "$root/.devkit" ] && [ ! -L "$root/.devkit" ] && projection=$(read_small "$root/.devkit/project.mk" 2>/dev/null); then
    apps=unavailable packages=unavailable
    while IFS= read -r line; do
      case "$line" in
        'DEVKIT_REGISTERED_APPS := '*) apps=${line#'DEVKIT_REGISTERED_APPS := '} ;;
        'DEVKIT_REGISTERED_PACKAGES := '*) packages=${line#'DEVKIT_REGISTERED_PACKAGES := '} ;;
      esac
    done <<EOF
$projection
EOF
    printf '\nRegistered apps: %s\nRegistered packages: %s\n' "${apps:-none}" "${packages:-none}"
  else
    printf '\nRegistered apps: unavailable\nRegistered packages: unavailable\n'
  fi
  exit 0
fi

boolean() { case "$2" in 0|1) ;; *) fail "$1 must be 0 or 1" ;; esac; }
value() { case "$2" in ''|-*) fail "set $1 to a value that does not start with a hyphen" ;; esac; }
slug() {
  printf '%s\n' "$2" | LC_ALL=C grep -xEq '[a-z][a-z0-9]*(-[a-z0-9]+)*' &&
    [ "$(printf '%s' "$2" | wc -l)" -eq 0 ] || fail "$1 must be a lowercase component name"
}
wrapped=0
if [ "$mode" = project ] && [ "$#" -eq 1 ]; then
  case "$command" in
    info|status)
      boolean json "${DEVKIT_INFO_JSON:-0}"
      set -- --non-interactive info "$root"
      if [ -n "${DEVKIT_APP:-}" ]; then
        slug app "$DEVKIT_APP"
        set -- "$@" --app "$DEVKIT_APP"
      fi
      [ -z "${DEVKIT_PACKAGE:-}" ] || fail 'info accepts only the app selector'
      [ "${DEVKIT_INFO_JSON:-0}" = 0 ] || set -- "$@" --json
      wrapped=1 ;;
    add-app|add-package|register|update|recover|sync)
      set -- --non-interactive "$command" "$root"
      case "$command" in
        add-app|add-package)
          slug name "${DEVKIT_LIFECYCLE_NAME:-}"
          case "$command:${DEVKIT_LIFECYCLE_TEMPLATE:-}" in
            add-app:node|add-app:node-app|add-app:node-api|add-app:rust|add-app:rust-app|add-app:rust-api|add-app:react|add-app:react-app|add-app:react-web|add-app:electron-react|add-package:ts-library|add-package:react-ui|add-package:react-client|add-package:rust-library) ;;
            *) fail 'set template to a supported id from make help' ;;
          esac
          set -- "$@" --name "$DEVKIT_LIFECYCLE_NAME" --template "$DEVKIT_LIFECYCLE_TEMPLATE"
          if [ "$command" = add-app ] && [ -n "${DEVKIT_LIFECYCLE_API:-}" ]; then
            slug api "$DEVKIT_LIFECYCLE_API"
            set -- "$@" --api "$DEVKIT_LIFECYCLE_API"
          fi
          set -f
          for dependency in ${DEVKIT_LIFECYCLE_USE:-}; do
            slug use "$dependency"
            set -- "$@" --use "$dependency"
          done
          set +f ;;
        register)
          case "${DEVKIT_APP:+app}${DEVKIT_PACKAGE:+package}" in
            app) slug app "$DEVKIT_APP"; set -- "$@" --app "$DEVKIT_APP" ;;
            package) slug package "$DEVKIT_PACKAGE"; set -- "$@" --package "$DEVKIT_PACKAGE" ;;
            *) fail 'register requires exactly one of app=NAME or package=NAME' ;;
          esac
          case "${DEVKIT_APP:+app}${DEVKIT_PACKAGE:+package}:${DEVKIT_LIFECYCLE_PROFILE:-}" in
            app:node|app:web|app:electron|app:rust|package:rust|package:ts-library|package:react-ui|package:react-library) ;;
            *) fail 'set profile to a supported id for the selected component' ;;
          esac
          set -- "$@" --profile "$DEVKIT_LIFECYCLE_PROFILE" ;;
        update)
          value to "${DEVKIT_LIFECYCLE_TO:-}"
          printf '%s\n' "$DEVKIT_LIFECYCLE_TO" | LC_ALL=C grep -xEq '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[a-z0-9][a-z0-9.-]*)?' &&
            [ "$(printf '%s' "$DEVKIT_LIFECYCLE_TO" | wc -l)" -eq 0 ] || fail 'to must be an exact release version'
          set -- "$@" --to "$DEVKIT_LIFECYCLE_TO" ;;
      esac
      if [ "$command" != recover ]; then
        boolean reconcile "${DEVKIT_SYNC_RECONCILE:-0}"
        [ "${DEVKIT_SYNC_RECONCILE:-0}" = 0 ] || set -- "$@" --reconcile-lock
      fi
      case "$command" in
        sync|update)
          boolean preview "${DEVKIT_SYNC_PREVIEW:-0}"
          [ "${DEVKIT_SYNC_PREVIEW:-0}" = 0 ] || set -- "$@" --preview ;;
      esac
      case "$command" in add-app|add-package|register) set -- "$@" --idempotent ;; esac
      wrapped=1 ;;
    exec)
      [ -z "${DEVKIT_EXEC_CONFLICT:-}" ] || fail 'choose either command= or cmd='
      [ -n "${DEVKIT_EXEC_COMMAND:-}" ] || fail 'exec requires command=ARGV or cmd=ARGV'
      boolean write "${DEVKIT_EXEC_WRITE:-0}"
      boolean prepare "${DEVKIT_EXEC_PREPARE:-0}"
      case "${DEVKIT_EXEC_NETWORK:-none}" in none|bridge) ;; *) fail 'network must be none or bridge' ;; esac
      set -- exec --root "$root" --network "${DEVKIT_EXEC_NETWORK:-none}"
      case "${DEVKIT_APP:+a}${DEVKIT_PACKAGE:+p}${DEVKIT_EXEC_COMPONENT:+c}" in
        '') ;;
        a) slug app "$DEVKIT_APP" ;;
        p) slug package "$DEVKIT_PACKAGE" ;;
        c)
          case "$DEVKIT_EXEC_COMPONENT" in
            apps/*|packages/*) slug component "${DEVKIT_EXEC_COMPONENT#*/}" ;;
            *) fail 'component must use apps/NAME or packages/NAME' ;;
          esac
          set -- "$@" --component "$DEVKIT_EXEC_COMPONENT" ;;
        *) fail 'choose at most one of app=, package=, or component=' ;;
      esac
      if [ -n "${DEVKIT_EXEC_TOOLCHAIN:-}" ]; then
        [ -z "${DEVKIT_APP:-}${DEVKIT_PACKAGE:-}${DEVKIT_EXEC_COMPONENT:-}" ] || fail 'remove toolchain= when selecting a component'
        case "$DEVKIT_EXEC_TOOLCHAIN" in node|rust) ;; *) fail 'toolchain must be node or rust' ;; esac
        set -- "$@" --toolchain "$DEVKIT_EXEC_TOOLCHAIN"
      fi
      [ "${DEVKIT_EXEC_WRITE:-0}" = 0 ] || set -- "$@" --write
      [ "${DEVKIT_EXEC_PREPARE:-0}" = 0 ] || set -- "$@" --prepare
      wrapped=1 ;;
  esac
fi
if [ "$mode" = project ] && [ "${DEVKIT_MAKE_DAG:-0}" = 1 ]; then
  [ ! -L "$root/.devkit" ] && [ ! -L "$root/.devkit/project.mk" ] \
    && [ ! -L "$root/.devkit/manifest.json" ] || fail 'accepted Make input contains a symlink'
  [ -n "${DEVKIT_MAKE_PLAN:-}" ] && [ -n "${DEVKIT_EXPECTED_MANIFEST:-}" ] \
    || fail 'cannot read accepted Make projection and manifest; run make recover or restore the managed files'
fi
cache_path() {
  lexical=$(realpath -ms -- "$1") || return 1
  [ "$(realpath -m -- "$1")" = "$lexical" ] || return 1
  printf '%s\n' "$lexical"
}
engine_metadata() {
  stat -c '%h/%d:%i:%s:%a/%z' -- "$engine"
}
engine_link_settled() {
  current_stamp=$(engine_metadata 2>/dev/null) || return 1
  before_inode=${engine_stamp#*/}
  after_inode=${current_stamp#*/}
  [ "${engine_stamp%%/*}" = 2 ] && [ "${current_stamp%%/*}" = 1 ] \
    && [ "${before_inode%%/*}" = "${after_inode%%/*}" ]
}
valid_engine_once() {
  if [ "$host_os" = Darwin ]; then
    require_755=1
    [ -z "${DEVKIT_ENGINE_PATH:-}" ] || require_755=0
    darwin_file verify "$engine" 134217728 "$expected" "$require_755"
    return
  fi
  [ -f "$engine" ] && [ ! -L "$engine" ] && [ -x "$engine" ] || return 1
  engine_stamp=$(engine_metadata) || return 1
  if [ -z "${DEVKIT_ENGINE_PATH:-}" ]; then
    case "$engine_stamp" in *:755/*) ;; *) return 1 ;; esac
  fi
  meta=${engine_stamp#*/}
  meta=${meta#*:}
  meta=${meta#*:}
  size=${meta%%:*}
  [ "$size" -le 134217728 ] || return 1
  observed=$(timeout --kill-after=0.05s 2s sh -c '
    set -eu
    exec 3<"$1"
    fd=/proc/$$/fd/3
    [ "$(stat -Lc "%h/%d:%i:%s:%a/%z" -- "$fd")" = "$2" ]
    head -c 134217729 <&3 | sha256sum
    [ "$(stat -Lc "%h/%d:%i:%s:%a/%z" -- "$fd")" = "$2" ]
  ' sh "$engine" "$engine_stamp") || return 1
  [ "${observed%% *}" = "$expected" ] && [ "$(engine_metadata)" = "$engine_stamp" ]
}
valid_engine() {
  if [ "$host_os" = Darwin ]; then
    valid_engine_once
    return
  fi
  engine_stamp=
  valid_engine_once || { engine_link_settled && valid_engine_once; }
}
engine_ready=0
if [ -n "${DEVKIT_ENGINE_PATH:-}" ]; then
  engine=$DEVKIT_ENGINE_PATH
else
  cache=${DEVKIT_CACHE_DIR:-${XDG_CACHE_HOME:-${HOME:?HOME or XDG_CACHE_HOME is required}/.cache}/docouno/devkit}
  case "$cache" in /*) ;; *) fail 'the cache directory must be absolute' ;; esac
  if [ "$host_os" = Darwin ]; then
    rc=0
    parent=$(darwin_file resolve-verify "$cache/engines/$target/$version/$expected" 134217728 "$expected" 1) || rc=$?
    case "$rc" in
      0) engine=$parent/devkit; engine_ready=1 ;;
      1) fail 'the cache path contains a symbolic link' ;;
      2) engine=$parent/devkit ;;
      3) fail "cached engine integrity differs: $parent/devkit" ;;
      4) fail 'the filesystem root cannot be the cache directory' ;;
      *) fail 'cached engine verification timed out or failed' ;;
    esac
  else
    parent=$(cache_path "$cache/engines/$target/$version/$expected") || fail 'the cache path contains a symbolic link'
    case "$parent" in
      "/engines/$target/$version/$expected") fail 'the filesystem root cannot be the cache directory' ;;
    esac
    engine=$parent/devkit
  fi
fi
exec_prepare=0
exec_has_root=0
cli_offline=0
if [ "$command" = exec ]; then
  if [ "$#" -eq 1 ] && [ "${DEVKIT_EXEC_PREPARE:-0}" = 1 ]; then
    exec_prepare=1
  elif [ "$#" -gt 1 ]; then
    for arg in "$@"; do
      [ "$arg" != -- ] || break
      [ "$arg" != --prepare ] || exec_prepare=1
      [ "$arg" != --root ] || exec_has_root=1
    done
  fi
elif [ "$command" = cli ]; then
  for arg in "$@"; do
    [ "$arg" != -- ] || break
    [ "$arg" != --offline ] || cli_offline=1
  done
fi
if [ "$engine_ready" = 0 ] && ! valid_engine && ! { [ -e "$engine" ] && valid_engine; }; then
  [ ! -e "$engine" ] && [ ! -L "$engine" ] || fail "cached engine integrity differs: $engine"
  [ -z "${DEVKIT_ENGINE_PATH:-}" ] || fail 'DEVKIT_ENGINE_PATH does not select the accepted engine'
  case "$mode:$command" in
    bootstrap:*|project:setup|project:setup-node|project:setup-rust|project:doctor|project:info|project:status|project:down|project:sync|project:add-app|project:add-package|project:register|project:update|project:recover) ;;
    *) { [ "$command" = cli ] && [ "$cli_offline" = 0 ]; } || [ "$exec_prepare" = 1 ] || fail 'the accepted engine is unavailable; run make setup' ;;
  esac
  old_umask=$(umask)
  umask 077
  mkdir -p "$parent"
  temporary=$(mktemp "$parent/.engine.XXXXXXXX")
  download_pid=
  cleanup() {
    if [ -n "$download_pid" ]; then
      kill -TERM "$download_pid" 2>/dev/null || :
      wait "$download_pid" 2>/dev/null || :
    fi
    rm -f "$temporary"
  }
  trap cleanup 0
  trap 'exit 130' INT
  trap 'exit 143' TERM
  curl --disable --fail --silent --show-error --location --max-redirs 5 \
    --proto '=https' --proto-redir '=https' --connect-timeout 10 --max-time 90 \
    --max-filesize 134217728 --output "$temporary" \
    "https://github.com/docouno/devkit/releases/download/v$version/$asset" &
  download_pid=$!
  if wait "$download_pid"; then
    download_pid=
  else
    status=$?
    download_pid=
    exit "$status"
  fi
  if [ "$host_os" = Darwin ]; then
    observed=$(darwin_file hash "$temporary" 134217728)
  else
    observed=$(timeout --kill-after=0.05s 2s sha256sum -- "$temporary")
  fi
  [ "${observed%% *}" = "$expected" ] || fail 'downloaded engine checksum differs'
  chmod 755 "$temporary"
  # Creating this link is atomic and cannot replace a concurrent installation.
  ln "$temporary" "$engine" 2>/dev/null || :
  cleanup
  trap - 0 INT TERM
  umask "$old_umask"
  valid_engine || fail 'the accepted engine could not be installed'
fi
if [ "$mode" = bootstrap ]; then
  DEVKIT_BOOTSTRAP_INTERACTIVE=1
  export DEVKIT_BOOTSTRAP_INTERACTIVE
  exec "$engine" "$@"
fi
if [ "$wrapped" = 1 ]; then
  exec "$engine" "$@"
fi
if [ "$command" = cli ]; then
  shift
  [ "$#" -gt 0 ] || set -- --help
  exec "$engine" "$@"
fi
if [ "$command" = sync ]; then
  shift
  exec "$engine" sync "$@"
fi
if [ "$command" = exec ]; then
  shift
  if [ "$exec_has_root" = 1 ]; then
    exec "$engine" exec "$@"
  fi
  exec "$engine" exec --root "$root" "$@"
fi
exec "$engine" task "$command" --root "$root"
