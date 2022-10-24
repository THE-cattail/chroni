all:
	cargo build --release

install:$(TARGET)
	cp target/release/chroni $(TARGET)

windows:
	cargo build --release --target x86_64-pc-windows-gnu
