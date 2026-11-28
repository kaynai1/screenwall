"""在 github.com:443 被阻断时，用 Git Data API 把本地提交推上去。

用法：
    py -3 scripts/push_via_api.py [--remote origin] [--branch main]

凭据从系统凭据管理器取（`git credential fill`），不落盘、不打印。
只上传相对父提交有变化的 blob，未改动的子目录直接沿用远端 tree 的 sha，
所以一次推送只有几个请求。
"""

import argparse
import base64
import json
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.github.com"


def git(*args, check=True):
    result = subprocess.run(
        ["git", *args], capture_output=True, text=True, encoding="utf-8"
    )
    if check and result.returncode != 0:
        raise SystemExit(f"git {' '.join(args)} 失败：{result.stderr.strip()}")
    return result.stdout


def credential():
    """从凭据管理器取 github.com 的 token，不打印。"""
    proc = subprocess.run(
        ["git", "credential", "fill"],
        input="protocol=https\nhost=github.com\n\n",
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise SystemExit("取不到 GitHub 凭据，请先配置 git credential")
    token = None
    for line in proc.stdout.splitlines():
        if line.startswith("password="):
            token = line[len("password=") :]
    if not token:
        raise SystemExit("凭据里没有 password 字段")
    return token


class Pusher:
    def __init__(self, token, owner, repo):
        self.token = token
        self.owner = owner
        self.repo = repo
        self.blobs = {}

    def api(self, method, path, body=None, raw=False):
        url = path if path.startswith("http") else API + path
        data = None
        headers = {
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "screenwall-push",
        }
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"

        last = None
        for attempt in range(5):
            req = urllib.request.Request(url, data=data, headers=headers, method=method)
            try:
                with urllib.request.urlopen(req, timeout=60) as resp:
                    payload = resp.read()
                    return payload if raw else json.loads(payload or b"null")
            except urllib.error.HTTPError as error:
                detail = error.read().decode("utf-8", "replace")[:400]
                last = f"{error.code} {detail}"
                # 业务类错误重试也没用，直接抛。
                if error.code in (401, 403, 404, 409, 422):
                    raise SystemExit(f"{method} {path} -> {last}")
            except (urllib.error.URLError, TimeoutError) as error:
                last = str(error)
            time.sleep(1.5 * (attempt + 1))
        raise SystemExit(f"{method} {path} 重试后仍失败：{last}")

    def blob(self, commit, rel_path):
        """上传提交里这个文件的 blob，返回 `{sha, mode}`，按本地 sha 缓存。

        从提交对象取内容，不要读工作区文件：工作区可能被 autocrlf 换成 CRLF，
        上传后 tree 会和本地提交对不上。mode 也从提交里取，保留可执行位。
        """
        listing = git("ls-tree", commit, "--", rel_path).strip()
        if not listing:
            raise SystemExit(f"{commit[:8]} 里没有 {rel_path}")
        meta = listing.split("\t", 1)[0].split()
        mode, sha = meta[0], meta[2]
        if sha in self.blobs:
            return {"sha": self.blobs[sha], "mode": mode}
        data = subprocess.run(
            ["git", "cat-file", "blob", sha], capture_output=True, check=True
        ).stdout
        created = self.api(
            "POST",
            f"/repos/{self.owner}/{self.repo}/git/blobs",
            {
                "content": base64.b64encode(data).decode(),
                "encoding": "base64",
            },
        )
        self.blobs[sha] = created["sha"]
        return {"sha": created["sha"], "mode": mode}

    def build_tree(self, base_tree_sha, changes):
        """自下而上重建目录树。

        changes: {相对路径: blob sha 或 None(删除)}
        未改动的子目录沿用 base tree 里的 sha；未改动的文件从 base tree 取同名项。
        """
        # 把改动按目录分组；目录键用 posix 风格，根目录是 ""。
        by_dir = {}
        for path, sha in changes.items():
            parts = path.split("/")
            directory = "/".join(parts[:-1])
            by_dir.setdefault(directory, {})[parts[-1]] = sha

        cache = {}

        def remote_tree(sha):
            if sha not in cache:
                data = self.api(
                    "GET", f"/repos/{self.owner}/{self.repo}/git/trees/{sha}"
                )
                cache[sha] = {item["path"]: item for item in data["tree"]}
            return cache[sha]

        def make_tree(directory, base_sha):
            """重建 `directory` 这一层；base_sha 是这一层对应的远端树。

            子目录要拿它自己的 base sha 递归下去，用根树的 sha 会取错项。
            新增目录（远端还没有这一层）必须从 changes 里推出来，否则整棵
            新目录会被静默丢掉。
            """
            base = remote_tree(base_sha) if base_sha else {}
            local = by_dir.get(directory, {})
            prefix = f"{directory}/" if directory else ""
            # 这一层下面还有改动的子目录名。
            child_dirs = set()
            for key in changes:
                if not key.startswith(prefix):
                    continue
                rest = key[len(prefix) :]
                if "/" in rest:
                    child_dirs.add(rest.split("/", 1)[0])
            entries = []

            for name in sorted(set(base) | set(local) | child_dirs):
                child = f"{directory}/{name}" if directory else name
                existing = base.get(name)

                if name in local:
                    blob = local[name]
                    if blob is None:
                        # 显式删除：不写进新树即可。
                        continue
                    entries.append(
                        {
                            "path": name,
                            "mode": blob.get("mode")
                            or (existing or {}).get("mode", "100644"),
                            "type": "blob",
                            "sha": blob["sha"],
                        }
                    )
                elif name in child_dirs:
                    entries.append(
                        {
                            "path": name,
                            "mode": (existing or {}).get("mode", "040000"),
                            "type": "tree",
                            "sha": make_tree(
                                child,
                                existing["sha"]
                                if existing and existing["type"] == "tree"
                                else None,
                            ),
                        }
                    )
                elif existing:
                    entries.append(existing)

            created = self.api(
                "POST",
                f"/repos/{self.owner}/{self.repo}/git/trees",
                {"tree": entries},
            )
            return created["sha"]

        return make_tree("", base_tree_sha)


def changed_paths(parent, commit):
    """返回 {相对路径: 状态}，状态是 A/M/D/R 等。"""
    out = git(
        "diff-tree",
        "-r",
        "--no-commit-id",
        "--name-status",
        "--no-renames",
        parent,
        commit,
    )
    result = {}
    for line in out.splitlines():
        if not line.strip():
            continue
        status, path = line.split("\t", 1)
        result[path] = status[0]
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--remote", default="origin")
    parser.add_argument("--branch", default="main")
    args = parser.parse_args()

    remote_url = git("remote", "get-url", args.remote).strip()
    if "github.com" not in remote_url:
        raise SystemExit(f"远端不是 GitHub：{remote_url}")
    slug = remote_url.split("github.com", 1)[1].lstrip(":/")
    if slug.endswith(".git"):
        slug = slug[: -len(".git")]
    owner, repo = slug.split("/", 1)

    head = git("rev-parse", "HEAD").strip()

    pusher = Pusher(credential(), owner, repo)

    ref = pusher.api(
        "GET", f"/repos/{owner}/{repo}/git/ref/heads/{args.branch}"
    )
    remote_head = ref["object"]["sha"]
    print(f"远端 {args.branch} = {remote_head[:8]}")

    if remote_head == head:
        print("远端已是最新，无需推送")
        return

    remote_tree = pusher.api(
        "GET", f"/repos/{owner}/{repo}/git/commits/{remote_head}"
    )["tree"]["sha"]

    # 远端必须是本地 HEAD 的祖先，否则说明两边分叉了。
    # 但通过 API 推上去的提交本地没有对象，祖先检查会失败；这时按目录树
    # 找等价的本地提交（树相同即内容相同），再从它往后推。
    ancestor = subprocess.run(
        ["git", "merge-base", "--is-ancestor", remote_head, head],
        capture_output=True,
    )
    if ancestor.returncode == 0:
        base = remote_head
    else:
        # 本地没有这个提交对象时 git 会往 stderr 打 "Not a valid commit name"，
        # 那是预期内的，别让它看起来像失败。
        base = None
        for candidate in git("rev-list", head).split():
            tree = git("rev-parse", f"{candidate}^{{tree}}").strip()
            if tree == remote_tree:
                base = candidate
                break
        if base is None:
            raise SystemExit(
                f"远端 {remote_head[:8]} 的目录树在本地找不到对应提交，"
                "两边内容已分叉，先手动处理"
            )
        print(f"远端提交是 API 创建的，对应本地 {base[:8]}（目录树一致）")

    commits = git("rev-list", "--reverse", f"{base}..{head}").split()
    if not commits:
        print("远端内容已是最新，无需推送")
        return
    print(f"待推送 {len(commits)} 个提交")

    tree = remote_tree
    parent = remote_head
    local_parent = base

    for commit in commits:
        # 和本地父提交比（远端 sha 本地没有对象），但新提交挂到远端链上。
        paths = changed_paths(local_parent, commit)
        local_parent = commit
        changes = {}
        for path, status in paths.items():
            if status == "D":
                changes[path] = None
            else:
                changes[path] = pusher.blob(commit, path)
        tree = pusher.build_tree(tree, changes)

        message = git("log", "-1", "--format=%B", commit)
        created = pusher.api(
            "POST",
            f"/repos/{owner}/{repo}/git/commits",
            {"message": message, "tree": tree, "parents": [parent]},
        )
        print(f"  {commit[:8]} -> {created['sha'][:8]} ({len(changes)} 个文件)")
        parent = created["sha"]

    pusher.api(
        "PATCH",
        f"/repos/{owner}/{repo}/git/refs/heads/{args.branch}",
        {"sha": parent, "force": False},
    )
    print(f"已推送：{args.branch} -> {parent}")


if __name__ == "__main__":
    sys.exit(main())
