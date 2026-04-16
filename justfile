name := 'cosmic-ext-applet-app-volume'
appid := 'io.github.ctsdownloads.CosmicExtAppletAppVolume'

prefix := '/usr'
bin-src := 'target/release/' + name
bin-dst := prefix + '/bin/' + name
desktop-src := 'res/' + appid + '.desktop'
desktop-dst := prefix + '/share/applications/' + appid + '.desktop'

default: build-release

# Run the applet from a terminal for debugging
run:
    cargo run

# Build optimized release binary
build-release:
    cargo build --release

# Build and install (requires sudo for /usr paths)
install: build-release
    sudo install -Dm0755 {{bin-src}} {{bin-dst}}
    sudo install -Dm0644 {{desktop-src}} {{desktop-dst}}

# Remove installed files
uninstall:
    sudo rm -f {{bin-dst}}
    sudo rm -f {{desktop-dst}}
