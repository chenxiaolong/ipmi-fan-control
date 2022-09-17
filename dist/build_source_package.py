#!/usr/bin/env python3

# Generates OS-specific packaging with metadata fields filled in.

import argparse
from dataclasses import dataclass
import glob
from functools import reduce
import hashlib
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
import textwrap
from typing import Optional, Type


@dataclass
class VersionInfo:
    # Base tag name
    version: str
    # Number of commits since tag (0 if building tag)
    plus_rev: str
    # git short commit ID of HEAD
    git_commit: str

    def suffix(self):
        suffix = ''

        if self.plus_rev:
            suffix += f'.r{self.plus_rev}'
        if self.git_commit:
            suffix += f'.git{self.git_commit}'

        return suffix

    def full(self):
        return self.version + self.suffix()


def compute_version(git_root_dir: os.PathLike) -> VersionInfo:
    raw_version = os.environ.get('VERSION_OVERRIDE') or \
        subprocess.check_output([
            'git', '-C', git_root_dir, 'describe', '--long'
        ]).decode().strip()

    components = raw_version.split('-')

    version = components[0].removeprefix('v')
    plus_rev = components[1] if len(components) > 1 else ''
    git_commit = components[2].removeprefix('g') if len(components) > 2 else ''

    return VersionInfo(
        version=version,
        plus_rev=plus_rev,
        git_commit=git_commit,
    )


def check_tools(tools: list[str]):
    missing = list(filter(lambda t: not shutil.which(t), tools))

    if missing:
        raise Exception('The following tools must be installed: ' +
                        ', '.join(sorted(missing)))


def replace_placeholders(path_in: os.PathLike, path_out: os.PathLike,
                         replacements: dict[str, str]):
    with open(path_in, 'r') as f:
        data = f.read()

    for source, target in replacements.items():
        data = data.replace(source, target)

    with open(path_out, 'w') as f:
        f.write(data)


def sha256sum(path: os.PathLike) -> str:
    m = hashlib.sha256()

    with open(path, 'rb') as f:
        while True:
            data = f.read(8192)
            if not data:
                break

            m.update(data)

    return m.hexdigest()


@dataclass
class Context:
    version: VersionInfo
    git_root_dir: os.PathLike
    output_dir: os.PathLike
    outputs: dict[Type, list[os.PathLike]]

    dsc_distro: Optional[str]
    dsc_suffix: str
    dsc_signed: bool

    def dist_dir(self) -> os.PathLike:
        return self.git_root_dir / 'dist'


class Action:
    @classmethod
    def all(cls) -> list[Type]:
        return cls.__subclasses__()

    def run(self, ctx: Context):
        os.makedirs(ctx.output_dir, exist_ok=True)

        with tempfile.TemporaryDirectory(dir=ctx.output_dir) as temp_dir:
            artifacts = self.build(ctx, pathlib.Path(temp_dir))
            outputs = []

            output_dir = ctx.output_dir / self.__class__.ID
            os.makedirs(output_dir, exist_ok=True)

            for artifact in artifacts:
                output_file = output_dir / artifact.name

                try:
                    os.unlink(output_file)
                except FileNotFoundError:
                    pass
                os.link(artifact, output_file)

                outputs.append(output_file)

            ctx.outputs[self.__class__] = outputs

    def build(self, ctx: Context, temp_dir: os.PathLike) -> list[os.PathLike]:
        raise NotImplementedError()


# Build tarball to be used for all distro's packaging
class TarballAction(Action):
    ID = 'tarball'
    DEPS = set()
    TOOLS = set()

    def build(self, ctx: Context, temp_dir: os.PathLike) -> list[os.PathLike]:
        prefix = f'ipmi-fan-control-{ctx.version.full()}'
        tarball = temp_dir / f'{prefix}.vendored.tar.xz'

        staging_dir = temp_dir / prefix
        os.makedirs(staging_dir)

        cmd = [
            'git',
            '-C', ctx.git_root_dir,
            'archive',
            '--format', 'tar',
            'HEAD',
        ]
        proc = subprocess.Popen(cmd, stdout=subprocess.PIPE)

        with tarfile.open(fileobj=proc.stdout, mode='r|') as tar:
            tar.extractall(staging_dir)

        if proc.wait() != 0:
            raise subprocess.CalledProcessError(proc.returncode, cmd)

        # Include all dependencies into the tarball because build services like
        # launchpad.net and build.opensuse.org do not allow internet access
        # during builds.
        subprocess.check_call(['cargo', 'vendor'], cwd=staging_dir)

        # Remove prebuilt winapi libraries
        globs = [
            staging_dir / 'vendor' / 'winapi-*' / 'lib',
            staging_dir / 'vendor' / 'windows_*' / 'lib',
        ]
        for g in globs:
            for path in glob.iglob(str(g)):
                shutil.rmtree(path)

        os.mkdir(staging_dir / '.cargo')
        shutil.copyfile(ctx.dist_dir() / 'cargo.vendored.toml',
                        staging_dir / '.cargo' / 'config')

        # Create a byte-for-byte reproducible tarball
        # See: https://reproducible-builds.org/docs/archives/
        subprocess.check_call([
            'tar',
            '-C', temp_dir,
            '--sort', 'name',
            '--mtime', '@0',
            '--owner', '0',
            '--group', '0',
            '--numeric-owner',
            '--pax-option',
            'exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime',
            '-Jcf', tarball,
            prefix
        ])

        return [tarball]


# Build source RPM for Fedora/CentOS
class SrpmAction(Action):
    ID = 'srpm'
    DEPS = {TarballAction}
    TOOLS = {'rpmbuild'}

    def build(self, ctx: Context, temp_dir: os.PathLike) -> list[os.PathLike]:
        for d in ('SOURCES', 'SPECS'):
            os.makedirs(temp_dir / d)

        replace_placeholders(
            ctx.dist_dir() / 'rpm' / 'ipmi-fan-control.spec.in',
            temp_dir / 'SPECS' / 'ipmi-fan-control.spec',
            {
                '@VERSION@': ctx.version.version,
                '@SUFFIX@': ctx.version.suffix(),
                '@TARBALL_NAME@': str(ctx.outputs[TarballAction][0].name),
            }
        )

        os.link(ctx.outputs[TarballAction][0],
                temp_dir / 'SOURCES' / ctx.outputs[TarballAction][0].name)

        subprocess.check_call([
            'rpmbuild',
            '--define', f'_topdir {temp_dir}',
            '-bs',
            temp_dir / 'SPECS' / 'ipmi-fan-control.spec',
        ])

        rpm_glob = temp_dir / 'SRPMS' / '*.src.rpm'
        rpm_file = next(glob.iglob(str(rpm_glob)))

        return [pathlib.Path(rpm_file)]


# Build PKGBUILD for Arch Linux
class PkgbuildAction(Action):
    ID = 'pkgbuild'
    DEPS = {TarballAction}
    TOOLS = set()

    def build(self, ctx: Context, temp_dir: os.PathLike) -> list[os.PathLike]:
        tarball = ctx.outputs[TarballAction][0]
        tarball_sha256 = sha256sum(tarball)

        replace_placeholders(
            ctx.dist_dir() / 'pkgbuild' / 'PKGBUILD.in',
            temp_dir / 'PKGBUILD',
            {
                '@VERSION@': ctx.version.full(),
                '@TARBALL_NAME@': tarball.name,
                '@TARBALL_SHA256@': tarball_sha256,
            }
        )

        return [temp_dir / 'PKGBUILD'] + ctx.outputs[TarballAction]


# Build deb source package for Debian/Ubuntu
class DscAction(Action):
    ID = 'debian'
    DEPS = {TarballAction}
    TOOLS = {'dch', 'debuild'}

    def build(self, ctx: Context, temp_dir: os.PathLike) -> list[os.PathLike]:
        # Debian/Ubuntu seem to prefer plusses over dots for git versions
        deb_full_version = ctx.version.version + \
            ctx.version.suffix().replace('.', '+')

        deb_tarball = temp_dir / \
            f'ipmi-fan-control_{deb_full_version}.orig.tar.xz'

        os.link(ctx.outputs[TarballAction][0], deb_tarball)

        with tarfile.open(deb_tarball, mode='r') as tar:
            tar.extractall(temp_dir)

        source_dir = temp_dir / f'ipmi-fan-control-{ctx.version.full()}'

        shutil.copytree(ctx.dist_dir() / 'debian',
                        source_dir / 'debian')

        dch_extra_args = []
        debuild_extra_args = []

        # The target distro and version suffix might be set to make the source
        # package uploadable.
        if ctx.dsc_distro:
            dch_extra_args.append('-D')
            dch_extra_args.append(ctx.dsc_distro)

        if not ctx.dsc_signed:
            dch_extra_args.append('-M')
            debuild_extra_args.append('-us')
            debuild_extra_args.append('-uc')

        # Create dummy changelog
        subprocess.check_call([
            'dch',
            '--create',
            '--package', 'ipmi-fan-control',
            '-v', f'{deb_full_version}-1{ctx.dsc_suffix}',
            *dch_extra_args,
            f'Automatically built from version {ctx.version.full()}',
        ], cwd=source_dir)

        # Skip cleaning because it requires additional dependencies, like
        # dh-exec, that prevent running this on non-Debian-based distros. We're
        # guaranteed to have a clean workspace already anyway.
        subprocess.check_call([
            'debuild', '-S', '-nc', *debuild_extra_args,
        ], cwd=source_dir)

        artifacts = []

        with os.scandir(temp_dir) as it:
            for entry in it:
                if entry.is_file():
                    artifacts.append(temp_dir / entry.name)

        return artifacts


def eval_action_deps(specified_actions):
    graph = {}

    def add_deps(action):
        if action not in graph:
            graph[action] = action.DEPS

            for dep in action.DEPS:
                add_deps(dep)

    for action in specified_actions:
        add_deps(action)

    order = []

    while graph:
        next_key = next(k for k in graph if not graph[k])
        order.append(next_key)
        del graph[next_key]

        for k in graph:
            graph[k].discard(next_key)

    return order


def parse_args():
    parser = argparse.ArgumentParser(
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=textwrap.dedent('''
            Debian-specific notes:
              The --dsc-distro and --dsc-suffix options are only useful when
              building Debian source packages that are meant to be uploaded to
              eg. launchpad.net where the same repo is used for multiple distro
              versions. --dsc-distro specifies the target distro name (eg.
              "focal") and --dsc-suffix specifies a version suffix (eg.
              "~ubuntu20.04") that makes the version number unique.

              If --dsc-signed is specified, then the resulting .dsc and .changes
              files will be signed. This requires DEBFULLNAME and DEBEMAIL to
              match the signing GPG key.
        '''),
    )

    target_group = parser.add_mutually_exclusive_group(required=True)
    target_group.add_argument('-t', '--target', action='append',
                              choices=[a.ID for a in Action.all()],
                              help='Type of source package to build')
    target_group.add_argument('-a', '--all-targets', action='store_true',
                              help='Build all source packages')

    debian_group = parser.add_argument_group('dsc-only options')
    debian_group.add_argument('--dsc-distro', metavar='NAME',
                              help='Target distro for dsc package upload')
    debian_group.add_argument('--dsc-suffix', metavar='SUFFIX', default='',
                              help='Version suffix for dsc package upload')
    debian_group.add_argument('--dsc-signed', action='store_true',
                              help='Create signed dsc/changes files')

    return parser.parse_args()


def main():
    args = parse_args()

    all_actions = {a.ID: a for a in Action.all()}
    if args.all_targets:
        actions = eval_action_deps({all_actions[t] for t in all_actions})
    else:
        actions = eval_action_deps({all_actions[t] for t in args.target})

    git_root_dir = pathlib.Path(subprocess.check_output([
        'git', '-C', sys.path[0], 'rev-parse', '--show-toplevel',
    ]).decode().removesuffix('\n'))

    version_info = compute_version(git_root_dir)
    print(f'Version: {version_info.version}')
    print(f'Commits since tag: {version_info.plus_rev}')
    print(f'HEAD short commit: {version_info.git_commit}')

    script_dir = pathlib.Path(sys.path[0])
    output_dir = script_dir / 'output'

    ctx = Context(
        version=version_info,
        git_root_dir=git_root_dir,
        output_dir=output_dir,
        outputs={},
        dsc_distro=args.dsc_distro,
        dsc_suffix=args.dsc_suffix,
        dsc_signed=args.dsc_signed,
    )

    # Check that tools exist in one go to avoid having the user need to install
    # packages more than once
    check_tools(reduce(lambda a, b: a.union(b),
                       (a.TOOLS for a in actions), set()))

    for ac in actions:
        ac().run(ctx)

    print('Outputs:')
    for path in sorted(p for outputs in ctx.outputs.values() for p in outputs):
        print('-', path)


if __name__ == '__main__':
    main()
