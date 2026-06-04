project(
    'clackd',
    'rust',
    version: '0.0.4',
    license: 'GPL-3.0-or-later',
    meson_version: '>= 0.59.0',
)

project_source_root = meson.project_source_root()

dep_systemd = dependency('systemd', required: get_option('systemd'))
enable_systemd = dep_systemd.found()

cargo = find_program('cargo', required: true)
cargo_path = cargo.full_path()

# Determine cargo target profile based on Meson's buildtype
cargo_target_dir = 'debug'
cargo_args = []
if get_option('buildtype') == 'release' or get_option('buildtype') == 'debugoptimized'
    cargo_target_dir = 'release'
    cargo_args += ['--release']
endif

# ── Binaries ─────────────────────────────────────────────────────────────
#
# clackd (daemon) and clackctl (CLI) are built by cargo and copied into the
# meson build dir for install. Both go in bindir: clackd is a session-scoped
# *user* service, not a privileged system daemon, so it does NOT belong in
# sbindir (CLAUDE.md §6).
#
# cargo manages its own incremental state under target/; the custom_target
# is intentionally always-built (cargo no-ops when nothing changed).
foreach bin : ['clackd', 'clackctl']
    custom_target(
        bin,
        build_by_default: true,
        command: [
            'sh',
            '-c',
            '_out="$PWD/$1" && cd "'
            + project_source_root
            + '" && "'
            + cargo_path
            + '" build ' + ' '.join(cargo_args) + ' --bin ' + bin
            + ' && cp target/' + cargo_target_dir + '/' + bin + ' "$_out"',
            '_',
            '@OUTPUT@',
        ],
        output: bin,
        install: true,
        install_dir: get_option('bindir'),
    )
endforeach

# ── Tests ────────────────────────────────────────────────────────────────
#
# `meson test` runs the crate's unit/integration suite via cargo. Hardware
# is not required — the tests use in-memory mocks.
test(
    'cargo-test',
    cargo,
    args: ['test'] + cargo_args,
    workdir: project_source_root,
    timeout: 300,
)

# ── udev rules ───────────────────────────────────────────────────────────
#
# Grants the active session unprivileged access to VIA hidraw nodes via
# TAG+="uaccess". Installed unconditionally; it is inert without matching
# hardware.
udev_dir = get_option('udev-dir')
if udev_dir == ''
    udev_dir = join_paths(get_option('prefix'), 'lib', 'udev')
endif
install_data(
    'dist/udev/60-clackd-via.rules',
    install_dir: join_paths(udev_dir, 'rules.d'),
)

# Substitution data shared by the configured service files: the absolute
# path to the installed clackd binary's directory.
config = configuration_data()
config.set('bindir', join_paths(get_option('prefix'), get_option('bindir')))

# ── systemd user unit ────────────────────────────────────────────────────
#
# Resolved the same way the libratbag canvas resolves its unit dir: honour
# an explicit override, else prefer pkg-config's systemduserunitdir, falling
# back to $libdir/systemd/user when the prefix diverges from the system one.
if enable_systemd
    unitdir = get_option('systemd-user-unit-dir')
    if unitdir == ''
        default_unitdir = dep_systemd.get_variable('systemduserunitdir')
        intended_unitdir = join_paths(get_option('prefix'), get_option('libdir'), 'systemd', 'user')
        if get_option('prefix') == '/usr' and intended_unitdir != default_unitdir
            message(
                'systemd user unitdir / libdir mismatch detected, using @0@'.format(default_unitdir),
            )
            unitdir = default_unitdir
        else
            unitdir = intended_unitdir
        endif
    endif

    configure_file(
        input: 'dist/systemd/clackd.service.in',
        output: 'clackd.service',
        configuration: config,
        install_dir: unitdir,
    )
endif

# ── D-Bus session activation ─────────────────────────────────────────────
#
# Lets the session bus lazily start clackd on first method call.
dbusdir = get_option('dbus-root-dir')
if dbusdir == ''
    dbusdir = join_paths(get_option('prefix'), get_option('datadir'), 'dbus-1')
endif

configure_file(
    input: 'dist/dbus/io.github.clackd.service.in',
    output: 'io.github.clackd.service',
    configuration: config,
    install_dir: join_paths(dbusdir, 'services'),
)

summary({
        'systemd user unit': enable_systemd,
        'bindir': join_paths(get_option('prefix'), get_option('bindir')),
    },
    section: 'clackd',
)
