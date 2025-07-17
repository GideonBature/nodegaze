# NodeGaze
NodeGaze: Unveiling Your Node's Universe.

NodeGaze is an open-source observability tool for Lightning node operators. Get unparalleled, real-time insight into your node's performance, channel health, and logs. It's implementation-agnostic (LND, c-lightning, Eclair, LDK) and offers a powerful Event Propagation API for real-time alerts and external integrations.
NodeGaze helps you truly see, understand, and master your Lightning node, setting the foundation for future provisioning services.

## Features

- **Multi-Implementation Support**: Works with LND, c-lightning, Eclair, and LDK
- **Real-time Monitoring**: Live insights into node performance and channel health
- **Event Propagation API**: Real-time alerts and external integrations
- **Observability Dashboard**: Comprehensive view of your Lightning node's universe
- **Open Source**: Fully open-source with community-driven development

## Development Setup

### Prerequisites

- Rust toolchain
- SQLite

> SQLx-cli you can install it using `cargo install sqlx-cli`

### Getting Started

1. **Clone the repository**
   ```bash
   git clone https://github.com/Extheoisah/nodegaze.git
   cd nodegaze
   ```

3. **Set up the database**
   ```bash
   # Create environment file by copying the example, make sure to udpate the values.
   cp .env.example .env

   # Run database migrations
   sqlx migrate run --source backend/migrations

   # Generate offline query data for SQLx
   cargo sqlx prepare --workspace
   ```

4. **Run the project**
   ```bash
   cargo run
   ```

### Database Management

The project uses SQLite with SQLx for database operations. Key commands:

- **Run migrations**: `sqlx migrate run --source backend/migrations`
- **Create new migration**: `sqlx migrate add <migration_name> --source backend/migrations`
- **Reset database**: `sqlx database drop && sqlx database create`
- **Generate offline data**: `cargo sqlx prepare --workspace`

### Environment Variables

Copy `.env.example` to `.env` and configure:

- `DATABASE_URL`: SQLite database path
- `DB_MAX_CONNECTIONS`: Maximum database connections (default: 5)
- `DB_ACQUIRE_TIMEOUT_SECONDS`: Connection timeout (default: 30)
- `ENCRYPTION_KEY`: Key for data encryption

### Development Tools

The Nix shell provides:

- **bacon**: Continuous testing and checking
- **sqlx-cli**: Database migrations and management
- **rust-analyzer**: LSP server for IDE support
- **rustfmt**: Code formatting
- **clippy**: Linting

## Contributing

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add some amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
```

This README provides:

- Clear project description and features
- Step-by-step development setup using the exact commands you used
- Database management instructions with SQLx commands
- Environment variable configuration
- Development tools explanation
- Contributing guidelines
- License information
