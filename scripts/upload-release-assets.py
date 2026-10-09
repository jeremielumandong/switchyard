#!/usr/bin/env python3
"""Create or extend the shared draft release for the workspace version and upload one
platform's assets to it.

    python3 scripts/upload-release-assets.py linux --check   # before compiling
    python3 scripts/upload-release-assets.py linux           # after packaging

Needs GH_TOKEN, GITHUB_REPOSITORY and GITHUB_SHA (set by GitHub Actions) and the `gh` CLI.
Runs on Python 3.10+ (the Linux release builds on Ubuntu 22.04).
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

PLATFORMS = ['linux', 'windows', 'macos']


def github_api(path, data=None, missing_ok=False):
    request = Request(
        os.environ.get('GITHUB_API_URL', 'https://api.github.com') + path,
        data=json.dumps(data).encode() if data is not None else None,
        headers={
            'Authorization': f'Bearer {os.environ["GH_TOKEN"]}',
            'Accept': 'application/vnd.github+json',
            'Content-Type': 'application/json',
            'User-Agent': 'switchyard-release',
        },
    )
    try:
        with urlopen(request, timeout=60) as response:
            return json.load(response)
    except HTTPError as error:
        if missing_ok and error.code == 404:
            return None
        raise


def find_release(api, repository, tag):
    # The tag endpoint only sees published releases; listing with push access also
    # includes drafts, even before the tag exists.
    page = 1
    while True:
        releases = api(f'/repos/{repository}/releases?per_page=100&page={page}')
        for release in releases:
            if release['tag_name'] == tag:
                return release
        if len(releases) < 100:
            return None
        page += 1


def check_release(api, repository, tag, sha):
    base = f'/repos/{repository}'
    release = find_release(api, repository, tag)
    if release is not None:
        if not release['draft']:
            raise ValueError(f'{tag} is already published. Bump the workspace version instead.')
        if release['target_commitish'] != sha:
            raise ValueError(f'{tag} targets a different commit. Every platform must build the same commit.')
    # An existing tag wins over target_commitish when GitHub publishes.
    reference = api(f'{base}/git/ref/tags/{tag}', missing_ok=True)
    if reference is not None:
        obj = reference['object']
        seen = set()
        while obj['type'] == 'tag':
            if obj['sha'] in seen:
                raise ValueError('Cyclic annotated release tag.')
            seen.add(obj['sha'])
            obj = api(f'{base}/git/tags/{obj["sha"]}')['object']
        if obj['type'] != 'commit' or obj['sha'] != sha:
            raise ValueError(f'Tag {tag} points to a different commit.')
    return release


def ensure_draft(api, repository, tag, sha):
    release = check_release(api, repository, tag, sha)
    if release is None:
        creation_error = None
        try:
            api(f'/repos/{repository}/releases', data={
                'tag_name': tag,
                'target_commitish': sha,
                'name': f'Switchyard {tag}',
                'draft': True,
                'generate_release_notes': True,
            })
        except HTTPError as error:
            if error.code != 422:
                raise
            creation_error = error
        # The list can lag behind a successful POST. Retry reads only: another POST can
        # create a duplicate draft. The workflows share a concurrency group for the same reason.
        for delay in (0, 1, 2, 4, 8, 16):
            if delay:
                time.sleep(delay)
            release = check_release(api, repository, tag, sha)
            if release is not None:
                break
        if release is None:
            if creation_error is not None:
                details = creation_error.read().decode('utf-8', errors='replace')
                raise ValueError(f'GitHub rejected draft {tag} (HTTP 422): {details}') from creation_error
            raise ValueError(f'Draft {tag} was created but is not visible yet. Retry; do not delete the draft.')
    return release


def sha256_of(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def release_assets(root, platform, version):
    dist = root / 'dist'
    if platform == 'linux':
        package = dist / f'Switchyard-{version}-x86_64.AppImage'
        stable_name = 'Switchyard-linux-x86_64.AppImage'
    elif platform == 'macos':
        package = dist / f'Switchyard-{version}-macos-universal.dmg'
        stable_name = 'Switchyard-macos-universal.dmg'
    else:
        package = dist / f'Switchyard-{version}-windows-x64-setup.exe'
        stable_name = 'Switchyard-windows-x64-setup.exe'
    assets = [package, package.with_name(package.name + '.sha256')]
    for asset in assets:
        if not asset.is_file() or asset.stat().st_size == 0:
            raise ValueError(f'Missing or empty release asset: {asset}')

    digest = sha256_of(package)
    checksum = assets[1].read_text(encoding='utf-8-sig').strip().split()
    if checksum != [digest, package.name]:
        raise ValueError(f'Checksum does not match {package.name}.')

    # A stable name for "latest" download links. Copying keeps the bytes (and the
    # Authenticode or notarization signature) identical.
    alias = dist / 'release-assets' / stable_name
    alias.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(package, alias)
    alias_checksum = alias.with_name(alias.name + '.sha256')
    alias_checksum.write_text(f'{digest}  {alias.name}\n', encoding='ascii')
    return assets + [alias, alias_checksum]


def upload(root, platform, version, repository, sha, api=github_api):
    assets = release_assets(root, platform, version)
    tag = f'v{version}'
    release = ensure_draft(api, repository, tag, sha)
    subprocess.run([
        'gh', 'release', 'upload', tag, '--repo', repository, '--clobber',
        *map(str, assets),
    ], check=True)
    return release


def workspace_version(root):
    # [workspace.package] version, read without tomllib (Python 3.10).
    section = None
    for line in (root / 'Cargo.toml').read_text(encoding='utf-8').splitlines():
        line = line.strip()
        if line.startswith('['):
            section = line
        elif section == '[workspace.package]':
            match = re.fullmatch(r'version\s*=\s*"([^"]+)"', line)
            if match:
                return match.group(1)
    raise ValueError('No [workspace.package] version in Cargo.toml.')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('platform', choices=PLATFORMS)
    parser.add_argument('--check', action='store_true', help='Check the release before compiling; do not upload.')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    version = workspace_version(root)
    if not re.fullmatch(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', version):
        raise ValueError('The release version must be a stable major.minor.patch.')
    repository = os.environ['GITHUB_REPOSITORY']
    sha = os.environ['GITHUB_SHA']
    if not re.fullmatch(r'[0-9a-f]{40}', sha):
        raise ValueError('GITHUB_SHA must identify an exact commit.')
    if args.check:
        check_release(github_api, repository, f'v{version}', sha)
        print(f'Release v{version} is ready for {args.platform} packaging at {sha}.')
        return
    release = upload(root, args.platform, version, repository, sha)
    message = (f'{args.platform.capitalize()} assets uploaded to draft v{version}: {release["html_url"]}\n'
               'Publish the draft once the Linux, Windows and macOS assets are attached.\n')
    print(message)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a', encoding='utf-8') as summary:
            summary.write(message)


if __name__ == '__main__':
    main()
