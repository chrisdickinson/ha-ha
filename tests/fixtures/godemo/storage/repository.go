package storage

// User is a user record.
type User struct {
	ID   uint64
	Name string
}

// Repository is the storage boundary the domain layer consumes.
type Repository interface {
	// FindUser finds a single user by id.
	FindUser(id uint64) (*User, error)
	// ListUsers lists every user.
	ListUsers() ([]User, error)
}
