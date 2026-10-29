"""Ported from ../asset-library/tests/test_library.py. Same assertions, black-box access."""

import io
import os
import zipfile

import pytest

from conftest import make_img, names, search, wait_idle


@pytest.fixture
def env(make_server, tmp_path):
    tokens = tmp_path / "tokens"
    make_img(tokens / "Undead" / "Skeleton_Archer.png", (255, 255, 255, 0))
    make_img(tokens / "Undead" / "zombie.png", (0, 128, 0, 255))
    make_img(tokens / "Goblins" / "goblin boss.webp", (200, 0, 0, 255), size=(512, 512))
    server = make_server()
    return server.client, tokens, tmp_path, server


def add_source(client, path):
    r = client.post("/api/sources", json={"path": str(path)})
    assert r.status_code == 200, r.text
    wait_idle(client)
    return r.json()["id"]


def rescan(client):
    client.post("/api/scan", json={})
    return wait_idle(client)


def test_scan_and_folder_tags(env):
    client, tokens, _, _ = env
    add_source(client, tokens)
    res = search(client, "")
    assert res["total"] == 3
    assert names(res) == ["Skeleton Archer", "goblin boss", "zombie"]
    tags = {t["name"]: t["count"] for t in client.get("/api/tags").json()}
    assert tags == {"goblins": 1, "undead": 2}


def test_search_syntax(env):
    client, tokens, _, _ = env
    add_source(client, tokens)
    assert names(search(client, "skel")) == ["Skeleton Archer"]          # prefix
    assert names(search(client, "tag:undead -zombie")) == ["Skeleton Archer"]
    assert names(search(client, "-tag:undead")) == ["goblin boss"]
    assert names(search(client, "w>=500")) == ["goblin boss"]
    assert names(search(client, "is:transparent")) == ["Skeleton Archer"]
    assert names(search(client, "goblins")) == ["goblin boss"]           # folder name
    assert search(client, 'bad "quote')["total"] == 0                    # no crash


def test_tagging_bulk_and_rename(env):
    client, tokens, _, _ = env
    add_source(client, tokens)
    ids = [i["id"] for i in search(client, "tag:undead")["items"]]
    client.post("/api/assets/bulk", json={"ids": ids, "add_tags": ["Low Level", "cr-1"]})
    assert names(search(client, "tag:low-level")) == ["Skeleton Archer", "zombie"]
    assert names(search(client, "low")) == ["Skeleton Archer", "zombie"]  # tags are searchable

    client.patch("/api/tags/cr-1", json={"new_name": "undead"})          # merge
    tags = {t["name"] for t in client.get("/api/tags").json()}
    assert "cr-1" not in tags and "undead" in tags

    client.post("/api/assets/bulk", json={"ids": ids[:1], "remove_tags": ["low-level"]})
    assert search(client, "tag:low-level")["total"] == 1

    client.patch(f"/api/assets/{ids[0]}", json={"notes": "uses a longbow", "favorite": True})
    assert search(client, "longbow is:fav")["total"] == 1


def test_untagged_and_type(env):
    client, tokens, _, _ = env
    client.post("/api/sources", json={"path": str(tokens), "folder_tags": False, "asset_type": "prop"})
    wait_idle(client)
    assert search(client, "is:untagged")["total"] == 3
    assert search(client, "type:prop")["total"] == 3
    assert search(client, "type:token")["total"] == 0


def test_move_keeps_tags(env):
    client, tokens, _, _ = env
    add_source(client, tokens)
    zid = search(client, "zombie")["items"][0]["id"]
    client.patch(f"/api/assets/{zid}", json={"tags": ["shambler"], "notes": "slow"})

    dest = tokens / "Reorganized" / "zombie-renamed.png"
    dest.parent.mkdir()
    os.replace(tokens / "Undead" / "zombie.png", dest)
    st = rescan(client)

    assert st["moved"] == 1 and st["missing"] == 0
    a = client.get(f"/api/assets/{zid}").json()
    assert a["rel_path"] == "Reorganized/zombie-renamed.png"
    assert a["tags"] == ["shambler"] and a["notes"] == "slow"
    assert search(client, "")["total"] == 3


def test_missing_files(env):
    client, tokens, _, _ = env
    add_source(client, tokens)
    (tokens / "Undead" / "zombie.png").unlink()
    rescan(client)
    assert search(client, "")["total"] == 2
    assert names(search(client, "is:missing")) == ["zombie"]


def test_thumbs_and_export(env):
    client, tokens, tmp, _ = env
    add_source(client, tokens)
    ids = [i["id"] for i in search(client, "")["items"]]
    r = client.get(f"/api/assets/{ids[0]}/thumb")
    assert r.status_code == 200 and r.headers["content-type"] == "image/webp"

    zf = zipfile.ZipFile(io.BytesIO(client.post("/api/export/zip", json={"ids": ids}).content))
    assert sorted(zf.namelist()) == ["Skeleton_Archer.png", "goblin boss.webp", "zombie.png"]

    out = tmp / "export"
    r = client.post("/api/export/folder", json={"ids": ids, "dest": str(out), "flatten": False})
    assert r.json()["count"] == 3
    assert (out / "tokens" / "Undead" / "zombie.png").exists()
    # exporting again doesn't overwrite
    client.post("/api/export/folder", json={"ids": ids[:1], "dest": str(out), "flatten": False})
    assert len(list(out.rglob("*.*"))) == 4


def test_remove_source_leaves_files(env):
    client, tokens, _, _ = env
    sid = add_source(client, tokens)
    client.delete(f"/api/sources/{sid}")
    assert search(client, "")["total"] == 0
    assert search(client, "zombie")["total"] == 0
    assert (tokens / "Undead" / "zombie.png").exists()


def test_skips_macos_junk_and_non_images(env):
    client, tokens, _, _ = env
    make_img(tokens / "__MACOSX" / "Undead" / "zombie.png", (0, 0, 0, 255))
    (tokens / "Undead" / "._zombie.png").write_bytes(b"\x00\x05\x16\x07 resource fork")
    (tokens / "Undead" / "corrupt.png").write_bytes(b"not really a png")
    add_source(client, tokens)
    assert search(client, "")["total"] == 3
    assert client.get("/api/scan/status").json()["skipped"] == 1   # corrupt.png
    assert "__macosx" not in {t["name"] for t in client.get("/api/tags").json()}


def test_rescan_prunes_previously_indexed_junk(env):
    client, tokens, _, server = env
    add_source(client, tokens)
    # Simulate a row indexed by an older version for a file we now exclude.
    junk = tokens / "__MACOSX" / "._x.png"
    junk.parent.mkdir()
    junk.write_bytes(b"junk")
    server.stop()
    with server.db() as conn:
        conn.execute("INSERT INTO assets (source_id, rel_path, file_hash, size, mtime, name) "
                     "VALUES (1, '__MACOSX/._x.png', 'h', 4, 0, '. x')")
    client = server.start().client
    wait_idle(client)
    assert search(client, "")["total"] == 4
    assert rescan(client)["removed"] == 1
    assert search(client, "")["total"] == 3


def test_thumbnails_warmed_after_scan(env):
    client, tokens, _, server = env
    add_source(client, tokens)
    st = client.get("/api/scan/status").json()["thumbs"]
    assert st["done"] == st["total"] == 3 and st["failed"] == 0
    for a in search(client, "")["items"]:
        assert server.thumb_path(a["file_hash"]).exists()
