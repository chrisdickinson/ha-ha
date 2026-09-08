package storage

/** A user record. */
case class User(id: Long, name: String)

/** The storage boundary the domain layer consumes. */
trait Repository:
  /** Find a single user by id. */
  def findUser(id: Long): Option[User]

  /** List every user. */
  def listUsers(): List[User]
