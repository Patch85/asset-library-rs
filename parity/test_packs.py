"""Dungeondraft packs (M9): images indexed in place inside .dungeondraft_pack files. Rust only."""

import io
import json
import os
import struct
import zipfile

import pytest
from PIL import Image

from conftest import names, rust_only, search, wait_idle

pytestmark = rust_only


def png(color, size=(48, 48)) -> bytes:
    buf = io.BytesIO()
    Image.new("RGBA", size, color).save(buf, "PNG")
    return buf.getvalue()


def write_pack(path, pack_id, meta, files, tags=None, trailing_comma=False):
    """A Godot 3 PCK like Dungeondraft writes: header, file table, then the files."""
    entries = [(f"res://packs/{pack_id}.json", json.dumps(meta).encode()),
               (f"res://packs/{pack_id}/pack.json", json.dumps(meta).encode())]
    if tags is not None:
        text = json.dumps({"tags": tags, "sets": {}})
        if trailing_comma:
            text = text.replace("]", ",]", 1)
        entries.append((f"res://packs/{pack_id}/data/default.dungeondraft_tags", text.encode()))
    entries += [(f"res://packs/{pack_id}/{name}", data) for name, data in files.items()]
    header = 4 + 16 + 64 + 4 + sum(4 + len(n.encode()) + 32 for n, _ in entries)
    out = bytearray(b"GDPC" + struct.pack("<4I", 1, 3, 2, 1) + bytes(64) + struct.pack("<I", len(entries)))
    offset = header
    for name, data in entries:
        out += struct.pack("<I", len(name.encode())) + name.encode() + struct.pack("<QQ", offset, len(data)) + bytes(16)
        offset += len(data)
    for _, data in entries:
        out += data
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(out))


CHAIR, BARREL, GRASS, WALL = (png((150, 90, 40, 255)), png((120, 70, 30, 255)),
                              png((60, 160, 60, 255), (64, 64)), png((90, 90, 90, 255), (64, 32)))


def make_pack(root, file_name="FA_Test_v1.dungeondraft_pack", **kw):
    write_pack(
        root / file_name, "FA30AB",
        {"name": kw.get("name", "FA Test"), "id": "FA30AB", "version": "1", "author": "FA"},
        {
            "textures/objects/Furniture/Chairs/Chair_Wood_A_1x1.png": CHAIR,
            "textures/objects/Containers/Barrel_A_1x1.png": BARREL,
            "textures/terrain/Grass_A.png": GRASS,
            "textures/walls/Wall_Stone_A.png": WALL,
            "textures/materials/notes.psd": b"8BPS not an image we index",
            "data/walls/stone.dungeondraft_wall": b"{}",
        },
        tags={"Furniture": ["textures/objects/Furniture/Chairs/Chair_Wood_A_1x1.png"],
              "[FA] Containers": ["textures/objects/Containers/Barrel_A_1x1.png"],
              "Colorable": ["textures/objects/Containers/Barrel_A_1x1.png"],
              ".Nature": ["textures/terrain/Grass_A.png"]},
        trailing_comma=True,
    )


@pytest.fixture
def lib(make_server, tmp_path):
    root = tmp_path / "dd"
    make_pack(root / "Forgotten Adventures")
    server = make_server()
    server.client.post("/api/sources", json={"path": str(root)})
    wait_idle(server.client)
    return server, root


def by_name(client):
    return {a["name"]: a for a in search(client, "", limit=100)["items"]}


def test_scan_types_tags_and_properties(lib):
    server, _ = lib
    c = server.client
    a = by_name(c)
    assert sorted(a) == ["Barrel A 1x1", "Chair Wood A 1x1", "Grass A", "Wall Stone A"]
    assert {n: x["asset_type"] for n, x in a.items()} == {
        "Barrel A 1x1": "prop", "Chair Wood A 1x1": "prop", "Grass A": "texture", "Wall Stone A": "other"}
    # Dungeondraft tags (cleaned, noise skipped) plus in-pack folders.
    assert a["Chair Wood A 1x1"]["tags"] == ["chairs", "furniture"]
    assert a["Barrel A 1x1"]["tags"] == ["containers"]
    assert a["Grass A"]["tags"] == ["nature"]
    assert a["Grass A"]["fields"] == {"kind": "terrain", "pack": "FA Test"}
    assert a["Wall Stone A"]["fields"] == {"kind": "wall", "pack": "FA Test"}
    assert a["Chair Wood A 1x1"]["fields"] == {"pack": "FA Test"}
    assert names(search(c, 'pack:"fa test" type:prop')) == ["Barrel A 1x1", "Chair Wood A 1x1"]
    assert names(search(c, "chair")) == ["Chair Wood A 1x1"]  # names and paths are searchable

    detail = c.get(f"/api/assets/{a['Chair Wood A 1x1']['id']}").json()
    assert detail["container"].endswith(".dungeondraft_pack")
    assert detail["full_path"].endswith("FA_Test_v1.dungeondraft_pack › textures/objects/Furniture/Chairs/Chair_Wood_A_1x1.png")
    assert (detail["width"], detail["height"]) == (48, 48)


def test_reads_bytes_thumbnails_and_exports(lib):
    server, root = lib
    c = server.client
    a = by_name(c)
    chair = a["Chair Wood A 1x1"]
    r = c.get(f"/api/assets/{chair['id']}/file")
    assert r.content == CHAIR and r.headers["content-type"] == "image/png"
    assert server.thumb_path(chair["file_hash"]).exists()
    assert c.get(f"/api/assets/{chair['id']}/thumb").status_code == 200

    ids = [x["id"] for x in a.values()]
    zf = zipfile.ZipFile(io.BytesIO(c.post("/api/export/zip", json={"ids": ids, "flatten": False}).content))
    assert "dd/Forgotten Adventures/FA_Test_v1/textures/objects/Furniture/Chairs/Chair_Wood_A_1x1.png" in zf.namelist()
    out = root.parent / "export"
    c.post("/api/export/folder", json={"ids": [chair["id"]], "dest": str(out)})
    assert (out / "Chair_Wood_A_1x1.png").read_bytes() == CHAIR


def test_updated_pack_keeps_edits(lib):
    server, root = lib
    c = server.client
    chair = by_name(c)["Chair Wood A 1x1"]
    c.patch(f"/api/assets/{chair['id']}", json={"notes": "good chair", "tags": ["seat"]})
    # A new version of the pack under a new file name, entries reordered (new offsets).
    old = root / "Forgotten Adventures" / "FA_Test_v1.dungeondraft_pack"
    os.remove(old)
    make_pack(root / "Forgotten Adventures", "FA_Test_v2.dungeondraft_pack", name="FA Test v2")
    c.post("/api/scan", json={})
    st = wait_idle(c)
    assert st["moved"] == 4 and st["missing"] == 0 and st["added"] == 0
    a = c.get(f"/api/assets/{chair['id']}").json()
    assert a["notes"] == "good chair" and a["tags"] == ["seat"]
    assert "FA_Test_v2" in a["rel_path"]
    assert c.get(f"/api/assets/{chair['id']}/file").content == CHAIR


def test_unchanged_pack_is_not_reread_and_removed_pack_goes_missing(lib):
    server, root = lib
    c = server.client
    c.post("/api/scan", json={})
    st = wait_idle(c)
    assert (st["processed"], st["added"], st["updated"], st["missing"]) == (4, 0, 0, 0)
    os.remove(root / "Forgotten Adventures" / "FA_Test_v1.dungeondraft_pack")
    c.post("/api/scan", json={})
    assert wait_idle(c)["missing"] == 4
    assert search(c, "")["total"] == 0 and search(c, "is:missing")["total"] == 4


def test_duplicate_images_in_two_packs(lib):
    server, root = lib
    c = server.client
    # Same chair bytes in a second pack: both stay, neither steals the other's record.
    write_pack(root / "Other.dungeondraft_pack", "OTHER1", {"name": "Other", "id": "OTHER1"},
               {"textures/objects/Chair_Copy.png": CHAIR})
    for _ in range(2):
        c.post("/api/scan", json={})
        wait_idle(c)
    assert names(search(c, "chair")) == ["Chair Copy", "Chair Wood A 1x1"]


def test_corrupt_pack_is_reported(make_server, tmp_path):
    root = tmp_path / "dd"
    root.mkdir()
    (root / "Broken.dungeondraft_pack").write_bytes(b"PK\x03\x04 actually a zip")
    c = make_server().client
    c.post("/api/sources", json={"path": str(root)})
    st = wait_idle(c)
    assert any("Broken.dungeondraft_pack" in e for e in st["errors"])


def test_export_every_search_result(lib):
    server, root = lib
    c = server.client
    out = root.parent / "all-props"
    r = c.post("/api/export/folder", json={"q": "type:prop", "dest": str(out), "flatten": False})
    assert r.json()["count"] == 2
    assert sorted(p.name for p in out.rglob("*.png")) == ["Barrel_A_1x1.png", "Chair_Wood_A_1x1.png"]
    assert c.post("/api/export/zip", json={"q": '~"chair"'}).status_code == 400  # ranked search


def test_sidebar_counts_follow_the_search(lib):
    server, _ = lib
    c = server.client
    every = c.get("/api/counts").json()
    assert every["types"] == {"prop": 2, "texture": 1, "other": 1}
    assert every["tags"] == {"chairs": 1, "furniture": 1, "containers": 1, "nature": 1}
    assert every["fields"] == {"pack": {"FA Test": 4}, "kind": {"terrain": 1, "wall": 1}}
    assert list(every["sources"].values()) == [4]

    props = c.get("/api/counts", params={"q": "type:prop -tag:containers"}).json()
    assert props["types"] == {"prop": 1}
    assert props["tags"] == {"chairs": 1, "furniture": 1}
    assert props["fields"] == {"pack": {"FA Test": 1}}
    assert c.get("/api/counts", params={"q": "nothing-matches-this"}).json()["tags"] == {}


def test_rescan_counts_every_source(lib):
    server, root = lib
    c = server.client
    tokens = root.parent / "tokens"
    tokens.mkdir()
    for i, color in enumerate([(200, 0, 0, 255), (0, 0, 200, 255)]):
        (tokens / f"t{i}.png").write_bytes(png(color))
    c.post("/api/sources", json={"path": str(tokens)})
    wait_idle(c)
    c.post("/api/scan", json={})
    st = wait_idle(c)
    assert (st["processed"], st["added"], st["errors"]) == (6, 0, [])  # 4 in the pack + 2 tokens
