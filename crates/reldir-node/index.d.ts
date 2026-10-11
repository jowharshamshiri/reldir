/** A row: column names to values. Integers beyond 2^53 are BigInts. */
export type Row = Record<string, unknown>;
export type Params = readonly unknown[] | Record<string, unknown> | undefined;

export interface OpenOptions {
  /** Write nothing at all, not even derived state. */
  readonly?: boolean;
  /** Adopt a folder of JSON files the first time (default true). */
  establish?: boolean;
  /** Seconds to wait for another writer. */
  wait?: number;
  /** Seconds a query may run. */
  timeout?: number;
  /** Rows a query may return. */
  maxRows?: number;
}

export interface ChangeOptions {
  /** Names the change, so a retry is reported rather than made twice. */
  idempotencyKey?: string;
  /** Widen tables' contracts to admit what the change writes. */
  widen?: boolean;
}

export interface Diagnostic {
  type: 'diagnostic';
  severity: 'error' | 'warning' | 'suggestion' | 'info';
  code: string;
  message: string;
  table?: string;
  path?: string;
  pointer?: string;
  help?: string;
  [member: string]: unknown;
}

export interface Changed {
  rows: number;
  induced: number;
  files: number;
  returning: Row[];
  revision: { number: number; id: string } | null;
  replayed: boolean;
  warnings: Diagnostic[];
}

export class ReldirError extends Error {
  code: string;
  exit: number;
  diagnostics: Diagnostic[];
}

export class Transaction {
  readonly active: boolean;
  query(sql: string, params?: Params): Promise<Row[]>;
  execute(sql: string, params?: Params): Promise<Changed>;
  insert(table: string, rows: Row | Row[]): Promise<Changed>;
  update(table: string, key: unknown, patch: Row): Promise<Changed>;
  delete(table: string, key: unknown): Promise<Changed>;
  commit(): Promise<Changed>;
  rollback(): Promise<null>;
}

export class Database {
  readonly root: string;
  query(sql: string, params?: Params): Promise<Row[]>;
  select(sql: string, params?: Params): Promise<{ columns: string[]; rows: Row[] }>;
  one(sql: string, params?: Params): Promise<Row | undefined>;
  scalar(sql: string, params?: Params): Promise<unknown>;
  execute(sql: string, params?: Params, options?: ChangeOptions): Promise<Changed>;
  insert(table: string, rows: Row | Row[]): Promise<Changed>;
  update(table: string, key: unknown, patch: Row): Promise<Changed>;
  delete(table: string, key: unknown): Promise<Changed>;
  get(table: string, key: unknown): Promise<Row | undefined>;
  tables(): Promise<{ name: string; rows: number; key: string[]; pinned: boolean }[]>;
  check(): Promise<{ valid: boolean; errors: Diagnostic[]; warnings: Diagnostic[] }>;
  state(): Promise<string>;
  begin(options?: ChangeOptions): Promise<Transaction>;
  transaction<T>(work: (tx: Transaction) => Promise<T>, options?: ChangeOptions): Promise<T>;
}

export function open(path: string, options?: OpenOptions): Promise<Database>;
export const version: string;
