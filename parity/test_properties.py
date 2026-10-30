"""Properties (M8): per-type fields with one value per asset. Rust only."""

import pytest

from conftest import make_img, names, rust_only, search, wait_idle

pytestmark = rust_only


@pytest.fixture
def lib(make_server, tmp_path):
    root = tmp_path / "tokens"
    make_img(root / "Undead" / "Zombie_Large_01.png", (60, 140, 60, 255), size=(80, 80))
    make_img(root / "Undead" / "Skeleton_Medium.png", (230, 230, 225, 255), size=(40, 40))
    make_img(root / "Undead" / "Ghoul.png", (120, 120, 90, 255), size=(40, 40))
    make_img(root / "Giants" / "Hill_Giant_Huge.png", (160, 110, 60, 255), size=(120, 120))
    make_img(root / "Giants" / "Stone_Giant.png", (150, 150, 150, 255), size=(120, 120))
    server = make_server()
    server.client.post("/api/sources", json={"path": str(root)})
    wait_idle(server.client)
    return server.client


def ids(client):
    return {a["name"]: a["id"] for a in search(client, "", limit=100)["items"]}


def field(client, key, asset_type="token"):
    return next(f for f in client.get("/api/fields").json() if f["key"] == key and f["asset_type"] == asset_type)


def test_default_fields(lib):
    keys = {(f["asset_type"], f["key"]) for f in lib.get("/api/fields").json()}
    assert {("token", "view"), ("token", "size"), ("token", "pack"),
            ("map", "grid"), ("map", "variant"), ("map", "pack")} <= keys


def test_set_values_and_search(lib):
    i = ids(lib)
    r = lib.patch(f"/api/assets/{i['Ghoul']}", json={"fields": {"size": "MEDIUM", "pack": "Forgotten Adventures"}})
    assert r.status_code == 200 and r.json()["fields"] == {"size": "medium", "pack": "Forgotten Adventures"}
    assert lib.patch(f"/api/assets/{i['Ghoul']}", json={"fields": {"size": "enormous"}}).status_code == 400
    assert lib.patch(f"/api/assets/{i['Ghoul']}", json={"fields": {"grid": 70}}).status_code == 400  # map only

    lib.post("/api/assets/bulk", json={"ids": [i["Hill Giant Huge"], i["Stone Giant"]], "set_fields": {"size": "huge"}})
    assert names(search(lib, "size:huge")) == ["Hill Giant Huge", "Stone Giant"]
    assert names(search(lib, "tag:undead -size:medium")) == ["Skeleton Medium", "Zombie Large 01"]
    assert names(search(lib, 'pack:"forgotten adventures"')) == ["Ghoul"]
    assert names(search(lib, "has:size")) == ["Ghoul", "Hill Giant Huge", "Stone Giant"]
    assert search(lib, "-has:size")["total"] == 2
    assert search(lib, "foo:bar")["total"] == 0  # unknown keys are still text

    lib.patch(f"/api/assets/{i['Ghoul']}", json={"fields": {"size": None}})
    assert "size" not in lib.get(f"/api/assets/{i['Ghoul']}").json()["fields"]


def test_numbers_and_type_change(lib):
    i = ids(lib)
    stone = i["Stone Giant"]
    lib.patch(f"/api/assets/{stone}", json={"fields": {"size": "huge", "pack": "FA"}})
    # Becoming a map keeps the shared key (pack) and drops the token-only one (size).
    a = lib.patch(f"/api/assets/{stone}", json={"asset_type": "map", "fields": {"grid": "70"}}).json()
    assert a["fields"] == {"grid": 70, "pack": "FA"}
    assert names(search(lib, "grid>=70")) == ["Stone Giant"]
    assert search(lib, "grid>70")["total"] == 0
    assert names(search(lib, "grid:70")) == ["Stone Giant"]


def test_facets(lib):
    i = ids(lib)
    lib.post("/api/assets/bulk", json={"ids": list(i.values()), "set_fields": {"view": "top-down"}})
    lib.patch(f"/api/assets/{i['Ghoul']}", json={"fields": {"size": "medium"}})
    lib.patch(f"/api/assets/{i['Zombie Large 01']}", json={"fields": {"size": "large"}})
    f = {x["key"]: x for x in lib.get("/api/facets").json()}
    assert f["view"]["values"] == [{"value": "top-down", "count": 5}]
    assert [v["value"] for v in f["size"]["values"]] == ["medium", "large"]  # option order


def test_edit_definitions(lib):
    r = lib.post("/api/fields", json={"asset_type": "token", "key": "is", "kind": "text"})
    assert r.status_code == 400  # reserved search word
    base = lib.post("/api/fields", json={"asset_type": "token", "key": "Base", "kind": "choice",
                                         "options": ["round", "square"]}).json()
    assert base["key"] == "base" and base["options"] == ["round", "square"]
    i = ids(lib)
    lib.patch(f"/api/assets/{i['Ghoul']}", json={"fields": {"base": "square"}})

    lib.patch(f"/api/fields/{base['id']}", json={"rename_value": {"from": "square", "to": "Hex"}})
    assert lib.get(f"/api/assets/{i['Ghoul']}").json()["fields"]["base"] == "Hex"
    lib.patch(f"/api/fields/{base['id']}", json={"remove_option": "hex"})
    assert "base" not in lib.get(f"/api/assets/{i['Ghoul']}").json()["fields"]

    lib.patch(f"/api/fields/{base['id']}", json={"key": "base-shape", "label": "Base shape", "position": 0})
    tokens = [f for f in lib.get("/api/fields").json() if f["asset_type"] == "token"]
    assert tokens[0]["key"] == "base-shape" and tokens[0]["label"] == "Base shape"
    lib.delete(f"/api/fields/{base['id']}")
    assert all(f["key"] != "base-shape" for f in lib.get("/api/fields").json())


def test_promote_tag(lib):
    r = lib.post("/api/tags/undead/promote", json={"field": "pack", "value": "Undead Pack"}).json()
    assert r == {"moved": 3, "skipped": 0}
    assert search(lib, 'pack:"undead pack"')["total"] == 3
    assert "undead" not in {t["name"] for t in lib.get("/api/tags").json()}
    assert lib.post("/api/tags/giants/promote", json={"field": "size", "value": "nope"}).status_code == 400


def suggestions(lib, aid):
    return {s["field"]: s for s in lib.post("/api/suggestions", json={"ids": [aid]}).json() if s.get("field")}


def test_suggestions(lib):
    i = ids(lib)
    s = suggestions(lib, i["Hill Giant Huge"])
    assert s["size"]["value"] == "huge" and s["size"]["source"] == "filename"

    lib.post("/api/assets/bulk", json={"ids": [i["Zombie Large 01"], i["Skeleton Medium"]],
                                       "set_fields": {"pack": "Undead Pack"}})
    s = suggestions(lib, i["Ghoul"])
    assert s["pack"]["value"] == "Undead Pack" and "same folder" in s["pack"]["source"]

    # Dismissing for this asset hides it; the suggestion entry names it key=value.
    lib.post("/api/suggestions/reject", json={"ids": [i["Ghoul"]], "tag": s["pack"]["tag"]})
    assert "pack" not in suggestions(lib, i["Ghoul"])
    # Set values aren't suggested.
    lib.patch(f"/api/assets/{i['Hill Giant Huge']}", json={"fields": {"size": "huge"}})
    assert "size" not in suggestions(lib, i["Hill Giant Huge"])


def test_same_size_suggestion(make_server, tmp_path):
    root = tmp_path / "tokens"
    for n in range(4):
        make_img(root / f"Pack{n}" / f"creature_{n}.png", (90, 40 * n, 30, 255), size=(80, 80))
    make_img(root / "Other" / "mystery.png", (10, 200, 30, 255), size=(80, 80))
    client = make_server().client
    client.post("/api/sources", json={"path": str(root)})
    wait_idle(client)
    i = ids(client)
    labelled = [i[f"creature {n}"] for n in range(4)]
    client.post("/api/assets/bulk", json={"ids": labelled, "set_fields": {"size": "large"}})
    s = suggestions(client, i["mystery"])
    assert s["size"]["value"] == "large" and s["size"]["source"] == "same size"


def test_bulk_edit_every_result(lib):
    r = lib.post("/api/assets/bulk", json={"q": "tag:undead", "set_fields": {"view": "top-down"}, "add_tags": ["horde"]})
    assert r.json() == {"ok": True, "count": 3}
    assert names(search(lib, "view:top-down tag:horde")) == ["Ghoul", "Skeleton Medium", "Zombie Large 01"]
    assert lib.post("/api/assets/bulk", json={"q": "nothing-matches", "favorite": True}).json()["count"] == 0
    assert lib.post("/api/assets/bulk", json={"q": '~"zombie"', "favorite": True}).status_code == 400
    assert search(lib, "is:fav")["total"] == 0
