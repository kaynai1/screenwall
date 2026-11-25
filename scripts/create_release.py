"""创建 GitHub Release 并上传附件（走 api.github.com / uploads.github.com）。

用法：
    py -3 scripts/create_release.py --tag v0.2.5 --name "ScreenWall 0.2.5" \
        --notes release-notes-0.2.5.md \
        --asset ScreenWall_0.2.5_x64-setup.exe=target/release/bundle/nsis/ScreenWall_0.2.5_x64-setup.exe \
        --asset screenwall-server-0.2.5.exe=target/release/screenwall-server.exe

凭据从系统凭据管理器取，不落盘、不打印。
"""

import argparse
import json
import mimetypes
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

API = "https://api.github.com"
UPLOADS = "https://uploads.github.com"


def git(*args):
    result = subprocess.run(["git", *args], capture_output=True, text=True)
    if result.returncode != 0:
        raise SystemExit(f"git {' '.join(args)} 失败：{result.stderr.strip()}")
    return result.stdout


def credential():
    proc = subprocess.run(
        ["git", "credential", "fill"],
        input="protocol=https\nhost=github.com\n\n",
        capture_output=True,
        text=True,
    )
    for line in proc.stdout.splitlines():
        if line.startswith("password="):
            return line[len("password=") :]
    raise SystemExit("取不到 GitHub 凭据")


class Client:
    def __init__(self, token, owner, repo):
        self.token = token
        self.owner = owner
        self.repo = repo
        self.base = f"/repos/{owner}/{repo}"

    def request(self, method, url, body=None, data=None, headers=None, retries=5):
        headers = dict(headers or {})
        headers.setdefault("Authorization", f"Bearer {self.token}")
        headers.setdefault("Accept", "application/vnd.github+json")
        headers.setdefault("User-Agent", "screenwall-release")
        if body is not None:
            data = json.dumps(body).encode()
            headers.setdefault("Content-Type", "application/json")

        last = None
        for attempt in range(retries):
            req = urllib.request.Request(url, data=data, headers=headers, method=method)
            try:
                with urllib.request.urlopen(req, timeout=300) as resp:
                    return json.loads(resp.read() or b"null")
            except urllib.error.HTTPError as error:
                detail = error.read().decode("utf-8", "replace")[:600]
                last = f"{error.code} {detail}"
                if error.code in (401, 403, 404):
                    raise SystemExit(f"{method} {url} -> {last}")
                if error.code == 422 and "already_exists" in detail:
                    raise
            except (urllib.error.URLError, TimeoutError) as error:
                last = str(error)
            time.sleep(2 * (attempt + 1))
        raise SystemExit(f"{method} {url} 重试后仍失败：{last}")

    def json(self, method, path, body=None):
        return self.request(method, API + self.base + path, body=body)

    def upload(self, upload_url, asset_path, name):
        ctype = mimetypes.guess_type(name)[0] or "application/octet-stream"
        with open(asset_path, "rb") as handle:
            payload = handle.read()
        # upload_url 形如 https://uploads.github.com/repos/o/r/releases/1/assets{?name,label}
        base = upload_url.split("{", 1)[0]
        url = f"{base}?name={urllib.request.quote(name)}"
        return self.request(
            "POST",
            url,
            data=payload,
            headers={"Content-Type": ctype},
        )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument("--notes", required=True)
    parser.add_argument("--target", default="main")
    parser.add_argument(
        "--asset",
        action="append",
        default=[],
        metavar="NAME=PATH",
        help="附件名=本地路径，可重复",
    )
    args = parser.parse_args()

    remote = git("remote", "get-url", "origin").strip()
    slug = remote.split("github.com", 1)[1].lstrip(":/")
    if slug.endswith(".git"):
        slug = slug[: -len(".git")]
    owner, repo = slug.split("/", 1)

    notes = open(args.notes, encoding="utf-8").read()
    client = Client(credential(), owner, repo)

    release = None
    try:
        release = client.json(
            "POST",
            "/releases",
            {
                "tag_name": args.tag,
                "target_commitish": args.target,
                "name": args.name,
                "body": notes,
                "draft": False,
                "prerelease": False,
            },
        )
        print(f"已创建 Release {args.tag} (id {release['id']})")
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")
        if error.code == 422 and "already_exists" in detail:
            release = client.json("GET", f"/releases/tags/{args.tag}")
            print(f"Release {args.tag} 已存在 (id {release['id']})，继续上传附件")
        else:
            raise SystemExit(f"创建 Release 失败：{error.code} {detail[:400]}")

    existing = {asset["name"]: asset for asset in release.get("assets", [])}
    for spec in args.asset:
        name, path = spec.split("=", 1)
        if name in existing:
            print(f"  附件已存在，跳过：{name}")
            continue
        asset = client.upload(release["upload_url"], path, name)
        print(f"  已上传 {name} ({asset['size']} B, digest={asset.get('digest')})")

    final = client.json("GET", f"/releases/tags/{args.tag}")
    print(f"Release 页面：{final['html_url']}")
    for asset in final["assets"]:
        print(f"  {asset['name']}: {asset['size']} B  {asset.get('digest')}")


if __name__ == "__main__":
    sys.exit(main())
