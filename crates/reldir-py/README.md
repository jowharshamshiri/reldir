# reldir for Python

The reldir engine in-process: a folder of JSON files as a relational database,
behind a [PEP 249](https://peps.python.org/pep-0249/) (DB-API 2.0) interface.
The wheel also installs the `reldir` command.

```python
import reldir

with reldir.connect("data") as db:          # commits on success, rolls back on error
    db.execute("UPDATE users SET age = age + 1 WHERE id = ?", ["ada"])
    for row in db.query("SELECT id, name FROM users ORDER BY name"):
        print(row["id"], row["name"])
```

Every change is validated against the tables' schemas and references before a
file is written, and a transaction is committed as one change or not at all.
See https://jowharshamshiri.github.io/reldir/python for the whole interface.
