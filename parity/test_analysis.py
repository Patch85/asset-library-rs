"""Ported from ../asset-library/tests/test_analysis.py. Same assertions, black-box access.

Fake embeddings are written into library.db with the server stopped (see
Server.inject_embeddings), since there is no in-process index to invalidate.
"""

import pytest

from conftest import make_img, models_available, search, wait_idle

FILES = {
    "Dragon/Red_Dragon_Large_01.png": (220, 30, 30, 255),
    "Dragon/Red_Dragon_Large_02.png": (210, 40, 35, 255),
    "Dragon/Blue_Dragon_Large_01.png": (230, 110, 190, 255),  # actually pink
    "Undead/Skeleton_Archer_01.png": (235, 235, 230, 255),
    "Undead/Skeleton_Warrior_01.png": (225, 225, 220, 255),
    "Pack17/wyrm_03.png": (215, 35, 30, 255),                 # untagged look-alike
    "Pack17/dragon_hatchling.png": (90, 90, 90, 255),
}


@pytest.fixture
def lib(make_server, tmp_path):
    """Library with colours analysed but no AI (embeddings injected by tests)."""
    root = tmp_path / "tokens"
    for rel, color in FILES.items():
        make_img(root / rel, color)
    server = make_server()
    server.client.post("/api/sources", json={"path": str(root)})
    wait_idle(server.client)
    return server


def ids_by_name(client):
    return {a["rel_path"].split("/")[-1]: a["id"]
            for a in client.get("/api/assets", params={"limit": 100}).json()["items"]}


def file_names(client, q):
    return sorted(a["rel_path"].split("/")[-1] for a in search(client, q)["items"])


def suggestions(client, ids):
    return client.post("/api/suggestions", json={"ids": ids}).json()


def test_color_profile(make_server, tmp_path):
    """Was a unit test of color_profile(); now checked through scan + analysis."""
    root = tmp_path / "tokens"
    make_img(root / "pink.png", (230, 110, 190, 255), size=(8, 8))
    make_img(root / "blue.png", (40, 80, 220, 255), size=(8, 8))
    make_img(root / "brown.png", (120, 70, 30, 255), size=(8, 8))
    make_img(root / "clear.png", (0, 0, 0, 0), size=(8, 8))
    client = make_server().client
    client.post("/api/sources", json={"path": str(root)})
    wait_idle(client)
    colors = {a["rel_path"]: client.get(f"/api/assets/{a['id']}").json()["colors"]
              for a in search(client, "")["items"]}
    assert max(colors["pink.png"].items(), key=lambda kv: kv[1])[0] == "pink"
    assert colors["blue.png"] == {"blue": 1.0}
    assert colors["brown.png"] == {"brown": 1.0}
    assert not colors["clear.png"]  # fully transparent


def test_color_search(lib):
    client = lib.client
    assert file_names(client, "color:pink") == ["Blue_Dragon_Large_01.png"]
    assert file_names(client, "tag:dragon color:red") == ["Red_Dragon_Large_01.png",
                                                          "Red_Dragon_Large_02.png"]
    assert "Blue_Dragon_Large_01.png" not in (file_names(client, "tag:dragon -color:pink")
                                              + file_names(client, "color:blue"))
    a = client.get(f"/api/assets/{ids_by_name(client)['Blue_Dragon_Large_01.png']}").json()
    assert next(iter(a["colors"])) == "pink"


def test_filename_suggestions(lib):
    client = lib.client
    ids = ids_by_name(client)
    by_tag = {s["tag"]: s for s in suggestions(client, [ids["dragon_hatchling.png"]])}
    assert by_tag["dragon"]["source"] == "filename" and by_tag["dragon"]["score"] == 0.9


def test_lookalike_suggestions_and_rejects(lib):
    red, white = [1, 0, 0, 0], [0, 1, 0, 0]
    lib.inject_embeddings({
        "Red_Dragon_Large_01.png": red, "Red_Dragon_Large_02.png": [0.95, 0.05, 0, 0],
        "Blue_Dragon_Large_01.png": [0.9, 0, 0.1, 0],
        "Skeleton_Archer_01.png": white, "Skeleton_Warrior_01.png": [0.05, 0.95, 0, 0],
        "wyrm_03.png": [0.97, 0.02, 0, 0], "dragon_hatchling.png": [0, 0, 0, 1],
    })
    client = lib.client
    ids = ids_by_name(client)
    wyrm = ids["wyrm_03.png"]
    sugg = {s["tag"]: s for s in suggestions(client, [wyrm])}
    assert sugg["dragon"]["source"] == "look-alike"
    assert "undead" not in sugg
    assert not any(t.startswith("pack17") for t in sugg)  # its own tags aren't suggested

    # Dismiss for this asset only, then never-suggest globally.
    client.post("/api/suggestions/reject", json={"ids": [wyrm], "tag": "dragon"})
    assert "dragon" not in {s["tag"] for s in suggestions(client, [wyrm])}
    hatch = ids["dragon_hatchling.png"]
    assert "dragon" in {s["tag"] for s in suggestions(client, [hatch])}
    client.post("/api/suggestions/reject", json={"tag": "dragon", "everywhere": True})
    assert "dragon" not in {s["tag"] for s in suggestions(client, [hatch])}
    assert client.get("/api/suggestions/ignored").json() == ["dragon"]

    # Multi-select merges and reports which assets each suggestion applies to.
    client.delete("/api/suggestions/ignored/dragon")
    merged = {s["tag"]: s for s in suggestions(client, [wyrm, hatch])}
    assert merged["dragon"]["ids"] == [hatch]  # wyrm's rejection still stands


def test_similar_search(lib):
    # Every file gets a vector so an AI-enabled server has nothing left to embed.
    other = [0, 0, 1]
    lib.inject_embeddings({
        "Red_Dragon_Large_01.png": [1, 0, 0], "Red_Dragon_Large_02.png": [0.9, 0.1, 0],
        "wyrm_03.png": [0.8, 0.3, 0], "Skeleton_Archer_01.png": [0, 1, 0],
        "Blue_Dragon_Large_01.png": other, "Skeleton_Warrior_01.png": other,
        "dragon_hatchling.png": other,
    })
    client = lib.restart(ai=True).client
    wait_idle(client)
    ids = ids_by_name(client)
    res = search(client, f"similar:{ids['Red_Dragon_Large_01.png']}")
    order = [a["rel_path"].split("/")[-1] for a in res["items"]]
    assert order[:3] == ["Red_Dragon_Large_01.png", "Red_Dragon_Large_02.png", "wyrm_03.png"]
    # Filters still apply on top of ranking.
    res = search(client, f"similar:{ids['Red_Dragon_Large_01.png']} -tag:pack17")
    assert "wyrm_03.png" not in [a["rel_path"].split("/")[-1] for a in res["items"]]


@pytest.mark.skipif(not models_available(), reason="CLIP model not in models/")
def test_real_model_text_search(make_server, tmp_path):
    root = tmp_path / "tokens"
    make_img(root / "a" / "one.png", (220, 30, 30, 255))
    make_img(root / "a" / "two.png", (40, 80, 220, 255))
    client = make_server(ai=True).client
    client.post("/api/sources", json={"path": str(root)})
    s = wait_idle(client)
    assert s["analysis"]["error"] == ""
    res = search(client, '~"a solid blue square"')
    assert res["items"][0]["rel_path"] == "a/two.png"
