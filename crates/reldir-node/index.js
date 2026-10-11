'use strict';
// @reldir/reldir: a folder of JSON files as a relational database, for Node.js.
//
//   const reldir = require('@reldir/reldir');
//   const db = await reldir.open('data');
//   const rows = await db.query('SELECT id, name FROM users WHERE age > ?', [30]);
//   await db.transaction(async (tx) => {
//     await tx.execute("UPDATE accounts SET balance = balance - 5 WHERE id = 'a'");
//     await tx.execute("UPDATE accounts SET balance = balance + 5 WHERE id = 'b'");
//   });
//
// Every call runs off the event loop and answers from the folder as it is
// then. Values cross exactly: an integer beyond Number.MAX_SAFE_INTEGER comes
// back as a BigInt, and a BigInt may be written.

// The native module for this platform, found by the loader napi generates.
const native = require('./native.js');

/** A reldir failure: `code` names the registered code, `diagnostics` every fault behind it. */
class ReldirError extends Error {
  constructor(payload) {
    super(`${payload.code}: ${payload.message}`);
    this.name = 'ReldirError';
    this.code = payload.code;
    this.exit = payload.exit;
    this.diagnostics = payload.diagnostics;
  }
}

function encode(value) {
  return JSON.stringify(value === undefined ? null : value, (_key, item) =>
    typeof item === 'bigint' ? JSON.rawJSON(item.toString()) : item,
  );
}

function decode(text) {
  return JSON.parse(text, (_key, value, context) => {
    if (typeof value === 'number' && !Number.isSafeInteger(value) && /^-?\d+$/.test(context.source)) {
      return BigInt(context.source);
    }
    return value;
  });
}

function rethrow(error) {
  let payload;
  try {
    payload = JSON.parse(error.message);
  } catch {
    throw error;
  }
  if (payload && typeof payload.code === 'string') {
    throw new ReldirError(payload);
  }
  throw error;
}

async function call(run) {
  try {
    return decode(await run());
  } catch (error) {
    return rethrow(error);
  }
}

function usage(message) {
  return new ReldirError({ code: 'USAGE', exit: 1, message, diagnostics: [] });
}

class Transaction {
  #inner;

  constructor(inner) {
    this.#inner = inner;
  }

  /** Whether it is still open. */
  get active() {
    return this.#inner.active;
  }

  /** Rows a read answers, reading what the transaction has done. */
  async query(sql, params) {
    return (await call(() => this.#inner.query(sql, encode(params)))).rows;
  }

  async execute(sql, params) {
    return call(() => this.#inner.execute(sql, encode(params)));
  }

  async insert(table, rows) {
    return call(() => this.#inner.insert(table, encode(rows)));
  }

  async update(table, key, patch) {
    return call(() => this.#inner.update(table, encode(key), encode(patch)));
  }

  async delete(table, key) {
    return call(() => this.#inner.delete(table, encode(key)));
  }

  /** Commit everything as one change. */
  async commit() {
    return call(() => this.#inner.commit());
  }

  /** Discard everything. */
  async rollback() {
    return call(() => this.#inner.rollback());
  }
}

class Database {
  #inner;

  constructor(inner) {
    this.#inner = inner;
  }

  get root() {
    return this.#inner.root;
  }

  /** Every row a read statement answers, each an object. */
  async query(sql, params) {
    return (await call(() => this.#inner.query(sql, encode(params)))).rows;
  }

  /** A read's answer with its columns: `{ columns, rows }`. */
  async select(sql, params) {
    return call(() => this.#inner.query(sql, encode(params)));
  }

  /** The one row a read answers, or undefined; more than one is an error. */
  async one(sql, params) {
    const rows = await this.query(sql, params);
    if (rows.length > 1) {
      throw usage(`the query answered ${rows.length} rows, not one`);
    }
    return rows[0];
  }

  /** The single value of a one-row, one-column answer. */
  async scalar(sql, params) {
    const row = await this.one(sql, params);
    if (row === undefined) return undefined;
    const values = Object.values(row);
    if (values.length !== 1) {
      throw usage(`the query answered ${values.length} columns, not one`);
    }
    return values[0];
  }

  /** Run one change and commit it. */
  async execute(sql, params, options = {}) {
    return call(() => this.#inner.execute(sql, encode(params), encode(options)));
  }

  async insert(table, rows) {
    return call(() => this.#inner.insert(table, encode(rows)));
  }

  async update(table, key, patch) {
    return call(() => this.#inner.update(table, encode(key), encode(patch)));
  }

  async delete(table, key) {
    return call(() => this.#inner.delete(table, encode(key)));
  }

  async get(table, key) {
    const row = await call(() => this.#inner.get(table, encode(key)));
    return row === null ? undefined : row;
  }

  async tables() {
    return call(() => this.#inner.tables());
  }

  async check() {
    return call(() => this.#inner.check());
  }

  async state() {
    return call(() => this.#inner.state());
  }

  /** Begin a transaction; it holds the writer lock until it ends. */
  async begin(options = {}) {
    try {
      return new Transaction(await this.#inner.begin(encode(options)));
    } catch (error) {
      return rethrow(error);
    }
  }

  /** Run `work` in a transaction: committed when it resolves, rolled back when it throws. */
  async transaction(work, options = {}) {
    const tx = await this.begin(options);
    let value;
    try {
      value = await work(tx);
    } catch (error) {
      if (tx.active) await tx.rollback();
      throw error;
    }
    await tx.commit();
    return value;
  }
}

/** Open the database in a folder, adopting a folder of JSON files the first time. */
async function open(path, options = {}) {
  try {
    return new Database(await native.open(String(path), encode(options)));
  } catch (error) {
    return rethrow(error);
  }
}

module.exports = { open, Database, Transaction, ReldirError, version: native.version() };
