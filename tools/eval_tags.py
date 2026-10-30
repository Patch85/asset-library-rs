"""Score image-only tag suggestions on a library's tokens.

Whole subfolders are held out (5 folds), as if a new, badly named pack were added, and each
method guesses their tags from CLIP embeddings alone. Run it on a copy of a library:

    parity/.venv/bin/python tools/eval_tags.py <copy of library.db>

A: the current look-alike vote (20 nearest, weight = sim - min + 1e-3, kept at >= 0.35).
B: per-tag logistic regression. C: per-tag ridge regression.
Tags come from folder names, so they're incomplete (a human cleric under Catch-All Heroes
isn't tagged human): "right" is a lower bound for every method.
"""
import sqlite3, sys, collections, time
import numpy as np

db = sys.argv[1]
c = sqlite3.connect(db)
rows = c.execute("""select a.id, a.rel_path, f.embedding from assets a join image_features f on f.file_hash = a.file_hash
                    where a.asset_type = 'token' and a.missing = 0 and f.embedding is not null""").fetchall()
ids = [r[0] for r in rows]
X = np.stack([np.frombuffer(r[2], dtype="<f4") for r in rows]).astype(np.float32)
group = ['/'.join(r[1].split('/')[:2]) if r[1].count('/') >= 2 else r[1].split('/')[0] for r in rows]
tags = collections.defaultdict(set)
for aid, t in c.execute("select x.asset_id, t.name from asset_tags x join tags t on t.id = x.tag_id"):
    tags[aid].add(t)
pos = {a: i for i, a in enumerate(ids)}
# Learnable concepts: >= 25 tokens across >= 3 subfolders, no digits/pack codes.
by_tag = collections.defaultdict(set)
for a in ids:
    for t in tags[a]:
        by_tag[t].add(a)
vocab = sorted(t for t, s in by_tag.items()
               if len(s) >= 25 and len({group[pos[a]] for a in s}) >= 3 and not any(ch.isdigit() for ch in t))
Y = np.array([[t in tags[a] for t in vocab] for a in ids], dtype=np.float32)
print(f"{len(ids)} tokens, {len(set(group))} subfolders, {len(vocab)} tags: {', '.join(vocab)}\n")

rng = np.random.default_rng(0)
groups = sorted(set(group))
rng.shuffle(groups)
fold_of = {g: i % 5 for i, g in enumerate(groups)}
fold = np.array([fold_of[g] for g in group])


def knn_scores(Xtr, Ytr, Xte, k=20):
    S = Xte @ Xtr.T
    out = np.zeros((len(Xte), Ytr.shape[1]), dtype=np.float32)
    for i in range(len(Xte)):
        nn = np.argpartition(-S[i], k)[:k]
        s = S[i, nn]
        w = (s - s.min()).clip(0) + 1e-3
        out[i] = (w[:, None] * Ytr[nn]).sum(0) / w.sum()
    return out


def standardize(Xtr, Xte):
    mu, sd = Xtr.mean(0), Xtr.std(0) + 1e-6
    return (Xtr - mu) / sd, (Xte - mu) / sd


def sigmoid(z):
    return 1 / (1 + np.exp(-np.clip(z, -30, 30)))


def logreg(Xtr, Ytr, Xte, l2=1e-2, steps=300, lr=0.5):
    """One-vs-rest logistic regression, all tags at once, full-batch gradient descent."""
    Xtr, Xte = standardize(Xtr, Xte)
    Xb = np.hstack([Xtr, np.ones((len(Xtr), 1), np.float32)])
    W = np.zeros((Xb.shape[1], Ytr.shape[1]), np.float32)
    for _ in range(steps):
        G = sigmoid(Xb @ W) - Ytr
        W -= lr * (Xb.T @ G / len(Xb) + l2 * W)
    return sigmoid(np.hstack([Xte, np.ones((len(Xte), 1), np.float32)]) @ W)


def ridge(Xtr, Ytr, Xte, lam=50.0):
    """Least squares per tag (closed form), clipped to [0, 1]."""
    Xtr, Xte = standardize(Xtr, Xte)
    A = Xtr.T @ Xtr + lam * np.eye(Xtr.shape[1], dtype=np.float32)
    W = np.linalg.solve(A, Xtr.T @ (Ytr - Ytr.mean(0)))
    return (Xte @ W + Ytr.mean(0)).clip(0, 1)


def report(name, P, thr):
    # top-1 precision, recall@3, and precision/recall of everything scoring >= thr
    has = Y.sum(1) > 0
    top1 = P.argmax(1)
    p1 = Y[np.arange(len(Y)), top1][has].mean()
    top3 = np.argsort(-P, 1)[:, :3]
    r3 = np.array([Y[i, top3[i]].sum() / max(Y[i].sum(), 1) for i in range(len(Y))])[has].mean()
    sel = P >= thr
    prec = (Y[sel].sum() / sel.sum()) if sel.sum() else float("nan")
    rec = (Y * sel).sum() / Y.sum()
    print(f"{name:<26} top-1 right {p1:6.1%}   recall@3 {r3:6.1%}   "
          f"shown (>= {thr}): {sel.sum() / len(Y):4.1f}/token, {prec:6.1%} right, {rec:6.1%} of true tags")


Pk = np.zeros_like(Y)
Pl = np.zeros_like(Y)
Pr = np.zeros_like(Y)
tl = tr_ = 0
for f in range(5):
    tr, te = fold != f, fold == f
    Pk[te] = knn_scores(X[tr], Y[tr], X[te])
    t = time.time(); Pl[te] = logreg(X[tr], Y[tr], X[te]); tl += time.time() - t
    t = time.time(); Pr[te] = ridge(X[tr], Y[tr], X[te]); tr_ += time.time() - t
print(f"training time per fold: logistic {tl/5:.1f} s, ridge {tr_/5:.2f} s (all {len(vocab)} tags)")
report("A look-alike vote (now)", Pk, 0.35)
report("B logistic", Pl, 0.5)
report("C ridge", Pr, 0.35)
report("A+B average", (Pk + Pl) / 2, 0.4)

