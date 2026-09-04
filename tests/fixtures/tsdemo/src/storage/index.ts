/** A user record. */
export interface User {
  id: number;
  name: string;
}

/** The storage boundary the domain layer consumes. */
export interface Repository {
  /** Find a single user by id. */
  findUser(id: number): User | undefined;
  /** List every user. */
  listUsers(): User[];
}

/** Exported helper. */
export function openPool(url: string, max: number): string {
  return `${url}:${max}`;
}

function notExported(url: string): string {
  return url;
}
