'use strict';
// The Node binding, used as an application uses it.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const reldir = require('..');

const DIALECT = 'https://reldir.dev/schema/reldir-3';

function write(root, relative, value) {
  const file = path.join(root, relative);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function keyed(table, column = 'id') {
  return { table, identity: { source: 'body', columns: [column], naming: { rule: 'reldir' } } };
}

function folder() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'reldir-node-'));
  write(root, 'schema/users.json', {
    $schema: DIALECT, type: 'object',
    properties: { id: { type: 'string' }, name: { type: 'string' }, age: { type: 'integer', 'x-reldir-type': 'int' }, big: { type: 'integer' } },
    required: ['id', 'name'], additionalProperties: false, 'x-reldir': keyed('users'),
  });
  write(root, 'schema/posts.json', {
    $schema: DIALECT, type: 'object',
    properties: { id: { type: 'string' }, user_id: { type: 'string' } },
    required: ['id', 'user_id'], additionalProperties: false,
    'x-reldir': { ...keyed('posts'), foreignKeys: [{ from: ['user_id'], to: { table: 'users' }, onDelete: 'cascade' }] },
  });
  write(root, 'users/ada.json', { id: 'ada', name: 'Ada', age: 36 });
  write(root, 'users/bob.json', { id: 'bob', name: 'Bob', age: 41 });
  write(root, 'posts/p1.json', { id: 'p1', user_id: 'ada' });
  return root;
}

test('reads answer rows, and see an edit made a moment ago', async () => {
  const root = folder();
  const db = await reldir.open(root);
  assert.deepEqual(await db.query('SELECT id, name FROM users WHERE age > ? ORDER BY id', [30]), [
    { id: 'ada', name: 'Ada' },
    { id: 'bob', name: 'Bob' },
  ]);
  const empty = await db.select('SELECT id, name FROM users WHERE age > 99');
  assert.deepEqual(empty, { columns: ['id', 'name'], rows: [] });
  write(root, 'users/ada.json', { id: 'ada', name: 'Ada L', age: 36 });
  assert.equal(await db.scalar('SELECT name FROM users WHERE id = :id', { id: 'ada' }), 'Ada L');
});

test('values beyond 2^53 cross exactly, as BigInt', async () => {
  const db = await reldir.open(folder());
  const big = 12345678901234567890123n;
  await db.update('users', 'ada', { big });
  const row = await db.get('users', 'ada');
  assert.equal(row.big, big);
  assert.equal(await db.scalar('SELECT count(*) FROM users WHERE big = ?', [big]), 1);
  assert.equal(typeof row.age, 'number');
});

test('a change is validated, and a refusal is a ReldirError with its code', async () => {
  const root = folder();
  const db = await reldir.open(root);
  const before = fs.readFileSync(path.join(root, 'posts/p1.json'));
  await assert.rejects(db.execute("UPDATE posts SET user_id = 'ghost'"), (error) => {
    assert.ok(error instanceof reldir.ReldirError);
    assert.equal(error.code, 'FOREIGN_KEY_VIOLATION');
    assert.equal(error.diagnostics[0].code, 'FOREIGN_KEY_VIOLATION');
    return true;
  });
  assert.deepEqual(fs.readFileSync(path.join(root, 'posts/p1.json')), before);
  const done = await db.execute('UPDATE users SET age = age + 1 WHERE id = ? RETURNING age', ['bob']);
  assert.deepEqual(done.returning, [{ age: 42 }]);
  assert.ok(done.revision.number >= 1);
  const deleted = await db.delete('users', 'ada');
  assert.equal(deleted.rows, 2);
  assert.equal(deleted.induced, 1);
  await assert.rejects(db.query('SELECT * FROM nowhere'), { code: 'UNKNOWN_TABLE' });
});

test('a transaction reads its writes and commits once, or rolls back', async () => {
  const root = folder();
  const db = await reldir.open(root);
  const seen = await db.transaction(async (tx) => {
    await tx.execute("INSERT INTO posts (id, user_id) VALUES ('p2', 'cy')");
    await tx.insert('users', { id: 'cy', name: 'Cy' });
    assert.equal(fs.existsSync(path.join(root, 'users/cy.json')), false);
    return (await tx.query("SELECT count(*) AS n FROM posts WHERE user_id = 'cy'"))[0].n;
  });
  assert.equal(seen, 1);
  assert.ok(fs.existsSync(path.join(root, 'users/cy.json')));

  const state = await db.state();
  await assert.rejects(
    db.transaction(async (tx) => {
      await tx.execute('DELETE FROM posts');
      throw new Error('changed my mind');
    }),
    /changed my mind/,
  );
  const tx = await db.begin();
  await tx.execute("INSERT INTO posts (id, user_id) VALUES ('p3', 'nobody')");
  await assert.rejects(tx.commit(), { code: 'FOREIGN_KEY_VIOLATION' });
  assert.equal(tx.active, false);
  await assert.rejects(tx.execute('DELETE FROM posts'), /the transaction has ended/);
  assert.equal(await db.state(), state);
});

test('the event loop keeps running while a transaction waits for the lock', async () => {
  const root = folder();
  const db = await reldir.open(root, { wait: 30 });
  const first = await db.begin();
  await first.execute("UPDATE users SET age = 1 WHERE id = 'ada'");
  let ticks = 0;
  const timer = setInterval(() => { ticks += 1; }, 5);
  const second = db.transaction(async (tx) => {
    await tx.execute("UPDATE users SET age = age + 1 WHERE id = 'ada'");
  });
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.ok(ticks > 3, 'the loop was not blocked');
  await first.commit();
  await second;
  clearInterval(timer);
  assert.equal(await db.scalar("SELECT age FROM users WHERE id = 'ada'"), 2);
});

test('a readonly database changes nothing', async () => {
  const root = folder();
  const db = await reldir.open(root, { readonly: true });
  assert.equal(await db.scalar('SELECT count(*) FROM users'), 2);
  await assert.rejects(db.execute('DELETE FROM posts'), { code: 'READ_ONLY' });
  assert.equal(fs.existsSync(path.join(root, '.db')), false);
  await assert.rejects(reldir.open(path.join(root, 'missing')), { code: 'PATH_NOT_FOUND' });
});
