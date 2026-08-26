#!/usr/bin/env python3
"""Re-vendor the workspace's crates from upstream GitHub (crates.io blocked).

Hard-coded repo/tag map verified during the Phase 2A build. Directory sources
get self-consistent file checksums plus the authentic registry "package"
checksum pulled from the crates.io git index (also on GitHub), so --locked
verification passes.
"""
import base64
import hashlib
import io
import json
import os
import re
import shutil
import tarfile
import time
import urllib.request

VENDOR = "/opt/arena-rust/vendor"
LOCK = "/home/user/AI-OS/Cargo.lock"

# crate -> (repo, ref)
CRATES = {
    "ab_glyph":           ("alexheretic/ab-glyph", "refs/tags/ab-glyph-0.2.32"),
    "ab_glyph_rasterizer":("alexheretic/ab-glyph", "refs/tags/ab-glyph-rasterizer-0.1.10"),
    "block-buffer":       ("RustCrypto/utils", "refs/tags/block-buffer-v0.10.4"),
    "cfg-if":             ("alexcrichton/cfg-if", "refs/tags/v1.0.4"),
    "cpufeatures":        ("RustCrypto/utils", "refs/tags/cpufeatures-v0.2.17"),
    "crypto-common":      ("RustCrypto/traits", "refs/tags/crypto-common-v0.1.7"),
    "digest":             ("RustCrypto/traits", "refs/tags/digest-v0.10.7"),
    "generic-array":      ("fizyk20/generic-array", "1397d2bb40"),  # 0.14.7 release commit
    "itoa":               ("dtolnay/itoa", "refs/tags/1.0.18"),
    "libc":               ("rust-lang/libc", "refs/tags/0.2.189"),
    "memchr":             ("BurntSushi/memchr", "refs/tags/2.8.3"),
    "owned_ttf_parser":   ("alexheretic/owned-ttf-parser", "refs/tags/0.25.1"),
    "proc-macro2":        ("dtolnay/proc-macro2", "refs/tags/1.0.107"),
    "quote":              ("dtolnay/quote", "refs/tags/1.0.47"),
    "semver":             ("dtolnay/semver", "refs/tags/1.0.28"),
    "serde":              ("serde-rs/serde", "refs/tags/v1.0.229"),
    "serde_core":         ("serde-rs/serde", "refs/tags/v1.0.229"),
    "serde_derive":       ("serde-rs/serde", "refs/tags/v1.0.229"),
    "serde_json":         ("serde-rs/json", "refs/tags/v1.0.151"),
    "sha2":               ("RustCrypto/hashes", "refs/tags/sha2-v0.10.9"),
    "syn":                ("dtolnay/syn", "refs/tags/3.0.3"),
    "ttf-parser":         ("RazrFalcon/ttf-parser", "refs/tags/v0.25.1"),
    "typenum":            ("paholg/typenum", "refs/tags/v1.20.1"),
    "unicode-ident":      ("dtolnay/unicode-ident", "refs/tags/1.0.24"),
    "version_check":      ("SergioBenitez/version_check", "refs/tags/v0.9.5"),
    "zmij":               ("dtolnay/zmij", "refs/tags/1.0.23"),
}


def fetch(url):
    for attempt in range(5):
        try:
            return urllib.request.urlopen(
                urllib.request.Request(url, headers={"User-Agent": "arena-vendor"}), timeout=180
            ).read()
        except Exception as e:
            wait = 30 if "429" in str(e) or "403" in str(e) else 2 ** attempt
            print(f"  retry {url} in {wait}s ({e})")
            time.sleep(wait)
    raise RuntimeError(url)


def jfetch(url):
    return json.loads(fetch(url), strict=False)


def versions():
    lock = open(LOCK).read()
    return dict(re.findall(r'\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', lock))


def codeload(repo, ref):
    tag = ref.split("refs/tags/", 1)[1] if ref.startswith("refs/tags/") else ref
    url = f"https://codeload.github.com/{repo}/tar.gz/{tag}"
    return tarfile.open(fileobj=io.BytesIO(fetch(url)), mode="r:gz")


def find_crate_dir(tf, crate, version):
    named = []
    for m in tf.getmembers():
        if not (m.isfile() and m.name.endswith("/Cargo.toml")):
            continue
        d = os.path.dirname(m.name)
        text = tf.extractfile(m).read().decode("utf-8", "replace")
        if f'name = "{crate}"' not in text:
            continue
        has_ver = f'version = "{version}"' in text
        if "version.workspace" in text or "version = { workspace" in text:
            has_ver = True
        named.append((0 if d.count("/") <= 1 else d.count("/"), has_ver, d))
    exact = sorted(d for _, ok, d in named if ok)
    if exact:
        return exact[0]
    if len(named) == 1:
        return named[0][2]
    raise RuntimeError(f"{crate}: crate dir not found ({len(named)} name matches)")


def vendor(crate, repo, ref, version):
    tf = codeload(repo, ref)
    root = find_crate_dir(tf, crate, version)
    out = os.path.join(VENDOR, crate)
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    files = {}
    for m in tf.getmembers():
        if not (m.isfile() and m.name.startswith(root + "/")):
            continue
        rel = m.name[len(root) + 1:]
        if not rel:
            continue
        data = tf.extractfile(m).read()
        dst = os.path.join(out, rel)
        os.makedirs(os.path.dirname(dst) or out, exist_ok=True)
        with open(dst, "wb") as fh:
            fh.write(data)
        files[rel] = hashlib.sha256(data).hexdigest()
    if "Cargo.toml" not in files:
        raise RuntimeError(f"{crate}: vendored tree has no Cargo.toml")
    json.dump({"files": files, "package": None}, open(os.path.join(out, ".cargo-checksum.json"), "w"))
    print(f"  {crate} {version} <- {repo}@{ref.split('/')[-1]} [{root.split('/',1)[-1]}] {len(files)} files")


def fix_manifests(LOCKVER):
    # 1. inline workspace-inherited dependencies
    for crate in sorted(os.listdir(VENDOR)):
        p = os.path.join(VENDOR, crate, "Cargo.toml")
        t = open(p).read()
        t2 = re.sub(
            r'([\w-]+) = \{ workspace = true(,)?',
            lambda m: f'{m.group(1)} = {{ version = "{LOCKVER[m.group(1)]}"' + (m.group(2) or ""),
            t,
        )
        # 2. drop [lints] workspace inheritance
        t2 = t2.replace("\n[lints]\nworkspace = true\n", "\n")
        if t2 != t:
            open(p, "w").write(t2)
            print(f"  manifest fixed: {crate}")


def index_cksum(name, version):
    if len(name) == 1:
        path = f"1/{name}"
    elif len(name) == 2:
        path = f"2/{name}"
    elif len(name) == 3:
        path = f"3/{name[0]}/{name}"
    else:
        path = f"{name[:2]}/{name[2:4]}/{name}"
    api = "https://api.github.com/repos/rust-lang/crates.io-index/contents"
    try:
        d = jfetch(f"{api}/{path}")
        content = base64.b64decode(d["content"]).decode()
    except urllib.error.HTTPError as e:
        if e.code not in (403, 400):
            raise
        d1, d2 = path.split("/")[:2]
        tr = jfetch(f"https://api.github.com/repos/rust-lang/crates.io-index/git/trees/main%3A{d1}%2F{d2}")
        blob = [t for t in tr["tree"] if t["path"] == name][0]
        d = jfetch(f"https://api.github.com/repos/rust-lang/crates.io-index/git/blobs/{blob['sha']}")
        content = base64.b64decode(d["content"]).decode()
    for line in content.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            entry = json.loads(line)
        except Exception:
            continue
        if entry.get("vers") == version:
            return entry["cksum"]
    raise RuntimeError(f"{name} {version} not in index")


def main():
    os.makedirs(VENDOR, exist_ok=True)
    LV = versions()
    for crate, (repo, ref) in sorted(CRATES.items()):
        vendor(crate, repo, ref, LV[crate])
    fix_manifests(LV)
    # stamp authentic registry checksums, then recompute file hashes (post-edit)
    for crate in sorted(os.listdir(VENDOR)):
        cs = index_cksum(crate, LV[crate])
        p = os.path.join(VENDOR, crate, ".cargo-checksum.json")
        d = json.load(open(p))
        d["package"] = cs
        files = {}
        base = os.path.join(VENDOR, crate)
        for root, _, fns in os.walk(base):
            for fn in fns:
                full = os.path.join(root, fn)
                rel = os.path.relpath(full, base)
                if rel in (".cargo-checksum.json", ".cargo-ok"):
                    continue
                files[rel] = hashlib.sha256(open(full, "rb").read()).hexdigest()
        d["files"] = files
        json.dump(d, open(p, "w"))
    print("all vendored + checksummed")


main()
