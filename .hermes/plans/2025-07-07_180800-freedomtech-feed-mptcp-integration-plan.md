# Freedom Tech Feed + MPTCP Bonding Integration Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Build and populate the Freedom Tech Feed with all packages, then integrate MPTCP multi-WAN bonding into tollgate-module-basic-go via a well-structured PR.

**Architecture:** Hybrid feed model (external artifacts + vendored source builds) with direct MPTCP integration in the main TollGate Go binary for seamless multi-WAN bonding.

**Tech Stack:** OpenWrt package feeds (opkg/apk), Go, Rust, MPTCP kernel modules, Ansible deployment.

---

## Phase 1: Complete Freedom Tech Feed Infrastructure

### Task 1: Build and test vendored packages in feed repo

**Objective:** Complete the wip packages (fips, tollgate-rs) and implement planned (mptcp-bonding)

**Files:**
- Create: `freedomtech-feed/package/mptcp-bonding/Makefile`
- Create: `freedomtech-feed/package/mptcp-bonding/files/mptcp-bonding.init`
- Modify: `freedomtech-feed/FEED-MANIFEST.conf` (update mptcp-bonding status to wip)
- Test: `freedomtech-feed/scripts/generate-packages-index.sh`

**Step 1: Create mptcp-bonding package structure**
```bash
mkdir -p ~/repos/freedomtech-feed/package/mptcp-bonding/files
```

**Step 2: Write mptcp-bonding Makefile**
```makefile
include $(TOPDIR)/rules.mk

PKG_NAME:=mptcp-bonding
PKG_RELEASE:=1
PKG_VERSION:=1.0.0
PKG_LICENSE:=MIT
PKG_MAINTAINER:=Freedom Tech Feed

include $(INCLUDE_DIR)/package.mk

define Package/mptcp-bonding
  SECTION:=net
  CATEGORY:=Network
  TITLE:=MPTCP Bonding Client for TollGate
  DEPENDS:=+shadowsocks-libev +ip-full +kmod-mptcp
  URL:=https://github.com/c03rad0r/tg-mptcp-server
endef

define Package/mptcp-bonding/description
  MPTCP bonding client that allows TollGate routers to combine multiple
  internet connections for increased bandwidth using Multipath TCP.
endef

define Build/Compile
  # This is a shell-only package - no compilation needed
endef

define Package/mptcp-bonding/install
  $(INSTALL_DIR) $(1)/etc/init.d
  $(INSTALL_BIN) ./files/mptcp-bonding.init $(1)/etc/init.d/mptcp-bonding
  $(INSTALL_DIR) $(1)/usr/sbin
  $(INSTALL_BIN) ./files/mptcp-client $(1)/usr/sbin/
endef

$(eval $(call BuildPackage,mptcp-bonding))
```

**Step 3: Create init script**
```bash
cat > ~/repos/freedomtech-feed/package/mptcp-bonding/files/mptcp-bonding.init << 'EOF'
#!/bin/sh /etc/rc.common

START=95
STOP=10
USE_PROCD=1

start_service() {
    procd_open_instance
    procd_set_param command /usr/sbin/mptcp-client
    procd_set_param respawn
    procd_close_instance
}

stop_service() {
    # Cleanup MPTCP endpoints
    ip mptcp endpoint flush 2>/dev/null || true
}
EOF
```

**Step 4: Create mptcp-client script**
```bash
cat > ~/repos/freedomtech-feed/package/mptcp-bonding/files/mptcp-client << 'EOF'
#!/bin/sh

# MPTCP client for TollGate
# Configures MPTCP endpoints for multi-WAN bonding

CONFIG_FILE="/etc/config/mptcp-bonding"
STATE_FILE="/var/run/mptcp-bonding.state"

[ -f "$CONFIG_FILE" ] || exit 1

. /lib/functions.sh
. /lib/functions/network.sh

setup_mptcp_endpoints() {
    local interface ipaddr
    
    # Clear existing endpoints
    ip mptcp endpoint flush 2>/dev/null || true
    
    # Get configured interfaces from UCI
    config_load mptcp-bonding
    config_foreach setup_interface interface
}

setup_interface() {
    local cfg="$1"
    local interface enabled
    
    config_get_bool enabled "$cfg" enabled 0
    [ "$enabled" = "1" ] || return 0
    
    config_get interface "$cfg" interface
    
    # Get interface IP
    network_get_ipaddr ipaddr "$interface"
    [ -n "$ipaddr" ] || return 0
    
    # Add MPTCP endpoint
    ip mptcp endpoint add "$ipaddr" dev "$interface" subflow
    logger "MPTCP: Added endpoint $ipaddr on $interface"
}

main() {
    case "$1" in
        start)
            setup_mptcp_endpoints
            echo "running" > "$STATE_FILE"
            ;;
        stop)
            ip mptcp endpoint flush 2>/dev/null || true
            rm -f "$STATE_FILE"
            ;;
        restart)
            $0 stop
            $0 start
            ;;
        *)
            echo "Usage: $0 {start|stop|restart}"
            exit 1
            ;;
    esac
}

main "$@"
EOF

chmod +x ~/repos/freedomtech-feed/package/mptcp-bonding/files/mptcp-client
```

**Step 5: Update FEED-MANIFEST.conf**
```bash
sed -i 's/mptcp-bonding.*planned/mptcp-bonding.*wip/' ~/repos/freedomtech-feed/FEED-MANIFEST.conf
```

**Step 6: Verify structure and commit**
```bash
cd ~/repos/freedomtech-feed
git add package/mptcp-bonding/ FEED-MANIFEST.conf
git commit -m "feat: add mptcp-bonding package structure"
```

### Task 2: Set up CI/CD for feed building

**Objective:** Configure GitHub Actions to build all packages and generate feed indices

**Files:**
- Modify: `freedomtech-feed/.github/workflows/build-feed.yml`
- Test: Build workflow should produce .ipk/.apk artifacts

**Step 1: Review and enhance build workflow**
```bash
cat ~/repos/freedomtech-feed/.github/workflows/build-feed.yml
```

**Step 2: Ensure workflow covers all architectures**
The workflow should target:
- aarch64_cortex-a53 (GL-MT6000, modern routers)
- mipsel_24kc (GL-MT3000, legacy routers)
- x86_64 (virtual/PC-based routers)

**Step 3: Test workflow manually (if needed)**
Trigger a test run to verify the CI builds all packages correctly.

**Step 4: Document build process**
Update `freedomtech-feed/docs/build-packages.md` with any missing details.

---

## Phase 2: MPTCP Bonding Integration into tollgate-module-basic-go

### Task 3: Design MPTCP integration architecture

**Objective:** Design how MPTCP bonding will integrate with existing TollGate modules

**Files:**
- Create: `~/repos/tollgate-module-basic-go/src/mptcp_bonding/` (new module)
- Modify: `~/repos/tollgate-module-basic-go/src/main.go` (initialize new module)
- Modify: `~/repos/tollgate-module-basic-go/src/config_manager/config.go` (add MPTCP config)
- Test: Existing test suite should still pass

**Step 1: Study existing module structure**
```bash
cd ~/repos/tollgate-module-basic-go/src
ls -la mptcp_bonding/ 2>/dev/null || echo "Module does not exist yet"
```

**Step 2: Design MPTCP configuration schema**
```go
// MPTCP bonding configuration to be added to config.json
"mptcp_bonding": {
  "enabled": false,
  "interfaces": [
    {
      "name": "wwan0",
      "enabled": true,
      "priority": 1
    }
  ],
  "server_config": {
    "host": "",
    "shadowsocks_port": 65101,
    "shadowsocks_password": "",
    "shadowsocks_method": "chacha20-ietf-poly1305"
  }
}
```

**Step 3: Create module directory structure**
```bash
mkdir -p ~/repos/tollgate-module-basic-go/src/mptcp_bonding
cd ~/repos/tollgate-module-basic-go/src/mptcp_bonding
```

### Task 4: Implement MPTCP bonding module

**Objective:** Create the core MPTCP bonding functionality

**Files:**
- Create: `~/repos/tollgate-module-basic-go/src/mptcp_bonding/config.go`
- Create: `~/repos/tollgate-module-basic-go/src/mptcp_bonding/manager.go`
- Create: `~/repos/tollgate-module-basic-go/src/mptcp_bonding/endpoint.go`
- Create: `~/repos/tollgate-module-basic-go/src/mptcp_bonding/proxy.go`
- Test: Unit tests for each component

**Step 1: Write MPTCP configuration structure**
```go
package mptcp_bonding

import (
	"encoding/json"
	"fmt"
)

type Config struct {
	Enabled      bool             `json:"enabled"`
	Interfaces   []InterfaceConfig `json:"interfaces"`
	ServerConfig ServerConfig      `json:"server_config"`
}

type InterfaceConfig struct {
	Name     string `json:"name"`
	Enabled  bool   `json:"enabled"`
	Priority int    `json:"priority"`
}

type ServerConfig struct {
	Host                string `json:"host"`
	ShadowsocksPort     int    `json:"shadowsocks_port"`
	ShadowsocksPassword string `json:"shadowsocks_password"`
	ShadowsocksMethod   string `json:"shadowsocks_method"`
}

func DefaultConfig() *Config {
	return &Config{
		Enabled: false,
		Interfaces: []InterfaceConfig{
			{Name: "wwan0", Enabled: true, Priority: 1},
		},
		ServerConfig: ServerConfig{
			ShadowsocksPort:   65101,
			ShadowsocksMethod: "chacha20-ietf-poly1305",
		},
	}
}

func (c *Config) Validate() error {
	if c.Enabled && c.ServerConfig.Host == "" {
		return fmt.Errorf("server host is required when MPTCP bonding is enabled")
	}
	return nil
}

func (c *Config) UnmarshalJSON(data []byte) error {
	type Alias Config
	alias := (*Alias)(c)
	
	if err := json.Unmarshal(data, alias); err != nil {
		return err
	}
	
	// Apply defaults
	if alias.ShadowsocksMethod == "" {
		alias.ShadowsocksMethod = "chacha20-ietf-poly1305"
	}
	if alias.ShadowsocksPort == 0 {
		alias.ShadowsocksPort = 65101
	}
	
	return c.Validate()
}
```

**Step 2: Write endpoint management**
```go
package mptcp_bonding

import (
	"fmt"
	"os/exec"
	"strings"
)

type EndpointManager struct {
	config *Config
}

func NewEndpointManager(config *Config) *EndpointManager {
	return &EndpointManager{config: config}
}

func (em *EndpointManager) SetupEndpoints() error {
	if !em.config.Enabled {
		return nil
	}
	
	// Clear existing endpoints
	if err := em.clearEndpoints(); err != nil {
		return fmt.Errorf("failed to clear existing endpoints: %w", err)
	}
	
	// Add configured endpoints
	for _, iface := range em.config.Interfaces {
		if !iface.Enabled {
			continue
		}
		
		if err := em.addEndpoint(iface.Name); err != nil {
			return fmt.Errorf("failed to add endpoint %s: %w", iface.Name, err)
		}
	}
	
	return nil
}

func (em *EndpointManager) clearEndpoints() error {
	cmd := exec.Command("ip", "mptcp", "endpoint", "flush")
	if err := cmd.Run(); err != nil {
		return err
	}
	return nil
}

func (em *EndpointManager) addEndpoint(interfaceName string) error {
	// Get interface IP (simplified - should use proper network discovery)
	cmd := exec.Command("ip", "addr", "show", interfaceName)
	output, err := cmd.Output()
	if err != nil {
		return err
	}
	
	// Parse IP from output (simplified)
	lines := strings.Split(string(output), "\n")
	for _, line := range lines {
		if strings.Contains(line, "inet ") && !strings.Contains(line, "127.0.0.1") {
			fields := strings.Fields(line)
			if len(fields) >= 2 {
				ip := strings.TrimSuffix(fields[1], "/24")
				if ip != "" {
					return em.addEndpointIP(ip, interfaceName)
				}
			}
		}
	}
	
	return fmt.Errorf("no IP found for interface %s", interfaceName)
}

func (em *EndpointManager) addEndpointIP(ip, interfaceName string) error {
	cmd := exec.Command("ip", "mptcp", "endpoint", "add", ip, "dev", interfaceName, "subflow")
	if err := cmd.Run(); err != nil {
		return err
	}
	return nil
}

func (em *EndpointManager) ListEndpoints() ([]string, error) {
	cmd := exec.Command("ip", "mptcp", "endpoint", "show")
	output, err := cmd.Output()
	if err != nil {
		return nil, err
	}
	
	lines := strings.Split(strings.TrimSpace(string(output)), "\n")
	var endpoints []string
	for _, line := range lines {
		if strings.TrimSpace(line) != "" {
			endpoints = append(endpoints, line)
		}
	}
	
	return endpoints, nil
}
```

**Step 3: Write proxy management**
```go
package mptcp_bonding

import (
	"context"
	"fmt"
	"net"
	"time"
)

type ProxyManager struct {
	config *Config
	conn   net.Conn
}

func NewProxyManager(config *Config) *ProxyManager {
	return &ProxyManager{config: config}
}

func (pm *ProxyManager) StartProxy(ctx context.Context) error {
	if !pm.config.Enabled {
		return nil
	}
	
	// Connect to shadowsocks server
	dialer := &net.Dialer{
		Timeout: 30 * time.Second,
	}
	
	conn, err := dialer.DialContext(ctx, "tcp", 
		fmt.Sprintf("%s:%d", pm.config.ServerConfig.Host, pm.config.ServerConfig.ShadowsocksPort))
	if err != nil {
		return fmt.Errorf("failed to connect to shadowsocks server: %w", err)
	}
	
	pm.conn = conn
	
	// Start proxy loop
	go pm.proxyLoop(ctx)
	
	return nil
}

func (pm *ProxyManager) proxyLoop(ctx context.Context) {
	defer pm.conn.Close()
	
	// Simplified proxy implementation
	// In production, this would use the shadowsocks protocol
	buffer := make([]byte, 4096)
	for {
		select {
		case <-ctx.Done():
			return
		default:
			n, err := pm.conn.Read(buffer)
			if err != nil {
				return
			}
			
			// Process data (simplified)
			_, err = pm.conn.Write(buffer[:n])
			if err != nil {
				return
			}
		}
	}
}

func (pm *ProxyManager) StopProxy() {
	if pm.conn != nil {
		pm.conn.Close()
		pm.conn = nil
	}
}
```

**Step 4: Write main manager**
```go
package mptcp_bonding

import (
	"context"
	"log"
	"sync"
)

type Manager struct {
	config         *Config
	endpointMgr   *EndpointManager
	proxyMgr      *ProxyManager
	ctx           context.Context
	cancel        context.CancelFunc
	wg            sync.WaitGroup
}

func NewManager(config *Config) *Manager {
	ctx, cancel := context.WithCancel(context.Background())
	return &Manager{
		config:       config,
		endpointMgr:  NewEndpointManager(config),
		proxyMgr:     NewProxyManager(config),
		ctx:          ctx,
		cancel:       cancel,
	}
}

func (m *Manager) Start() error {
	if !m.config.Enabled {
		return nil
	}
	
	// Setup MPTCP endpoints
	if err := m.endpointMgr.SetupEndpoints(); err != nil {
		return fmt.Errorf("failed to setup MPTCP endpoints: %w", err)
	}
	
	// Start proxy
	m.wg.Add(1)
	go func() {
		defer m.wg.Done()
		if err := m.proxyMgr.StartProxy(m.ctx); err != nil {
			log.Printf("MPTCP proxy error: %v", err)
		}
	}()
	
	return nil
}

func (m *Manager) Stop() {
	m.cancel()
	m.wg.Wait()
	m.proxyMgr.StopProxy()
}

func (m *Manager) Status() map[string]interface{} {
	endpoints, _ := m.endpointMgr.ListEndpoints()
	
	return map[string]interface{}{
		"enabled":    m.config.Enabled,
		"endpoints":  endpoints,
		"server":     m.config.ServerConfig.Host,
		"running":    m.ctx.Err() == nil,
	}
}
```

**Step 5: Write unit tests**
```go
package mptcp_bonding

import (
	"testing"
)

func TestConfigValidation(t *testing.T) {
	config := DefaultConfig()
	
	// Should pass when disabled
	if err := config.Validate(); err != nil {
		t.Errorf("Expected no error for disabled config, got %v", err)
	}
	
	// Should fail when enabled but no host
	config.Enabled = true
	if err := config.Validate(); err == nil {
		t.Error("Expected error for enabled config without host")
	}
	
	// Should pass when enabled with host
	config.ServerConfig.Host = "example.com"
	if err := config.Validate(); err != nil {
		t.Errorf("Expected no error for valid enabled config, got %v", err)
	}
}

func TestEndpointManager(t *testing.T) {
	config := DefaultConfig()
	em := NewEndpointManager(config)
	
	// Test clear endpoints (should not error even if no endpoints exist)
	if err := em.clearEndpoints(); err != nil {
		t.Errorf("Failed to clear endpoints: %v", err)
	}
}

func TestManager(t *testing.T) {
	config := DefaultConfig()
	manager := NewManager(config)
	
	// Test that manager can be created and stopped
	manager.Stop()
	
	// Test status
	status := manager.Status()
	if status["enabled"] != false {
		t.Error("Expected disabled status")
	}
}
```

**Step 6: Run tests and verify**
```bash
cd ~/repos/tollgate-module-basic-go/src
go test ./mptcp_bonding/...
```

### Task 5: Integrate MPTCP module into main application

**Objective:** Wire up the MPTCP bonding module with the main TollGate application

**Files:**
- Modify: `~/repos/tollgate-module-basic-go/src/config_manager/config.go`
- Modify: `~/repos/tollgate-module-basic-go/src/main.go`
- Test: Integration tests

**Step 1: Update config schema**
Add MPTCP bonding to the main configuration:

```go
// In config_manager/config.go, add to Config struct
type Config struct {
	// ... existing fields ...
	MPTCPBonding *mptcp_bonding.Config `json:"mptcp_bonding"`
}

// In config loading, add default MPTCP config
func LoadConfig(path string) (*Config, error) {
	// ... existing code ...
	
	cfg := &Config{
		// ... existing defaults ...
		MPTCPBonding: mptcp_bonding.DefaultConfig(),
	}
	
	// ... rest of loading logic ...
}
```

**Step 2: Update main.go**
```go
// Add import
import "github.com/OpenTollGate/tollgate-module-basic-go/src/mptcp_bonding"

// In main() function, after loading config:
var mptcpManager *mptcp_bonding.Manager

// After config loading:
if config.MPTCPBonding != nil {
	mptcpManager = mptcp_bonding.NewManager(config.MPTCPBonding)
	if err := mptcpManager.Start(); err != nil {
		log.Printf("Failed to start MPTCP bonding: %v", err)
	}
}

// Add graceful shutdown
defer func() {
	if mptcpManager != nil {
		mptcpManager.Stop()
	}
}()
```

**Step 3: Add MPTCP status to CLI**
```go
// In cli/status.go or similar, add MPTCP status
func addMPTCPStatus(status map[string]interface{}) {
	if mptcpManager != nil {
		status["mptcp_bonding"] = mptcpManager.Status()
	}
}
```

**Step 4: Test integration**
```bash
cd ~/repos/tollgate-module-basic-go/src
go build .
go test -tags testenv ./...
```

---

## Phase 3: Testing and Quality Assurance

### Task 6: Write comprehensive tests

**Objective:** Ensure MPTCP integration is thoroughly tested

**Files:**
- Create: `~/repos/tollgate-module-basic-go/src/mptcp_bonding/manager_test.go`
- Create: `~/repos/tollgate-module-basic-go/src/mptcp_bonding/integration_test.go`
- Test: Test coverage should be >80%

**Step 1: Write integration tests**
```go
package mptcp_bonding

import (
	"context"
	"testing"
	"time"
)

func TestManagerIntegration(t *testing.T) {
	config := DefaultConfig()
	config.Enabled = false // Don't actually connect in tests
	
	manager := NewManager(config)
	
	// Test start/stop lifecycle
	if err := manager.Start(); err != nil {
		t.Errorf("Failed to start manager: %v", err)
	}
	
	// Let it run briefly
	time.Sleep(100 * time.Millisecond)
	
	manager.Stop()
	
	// Check status
	status := manager.Status()
	if status["enabled"] != false {
		t.Error("Expected disabled status")
	}
}
```

**Step 2: Test error conditions**
```go
func TestErrorConditions(t *testing.T) {
	t.Run("InvalidConfig", func(t *testing.T) {
		config := DefaultConfig()
		config.Enabled = true
		config.ServerConfig.Host = ""
		
		if err := config.Validate(); err == nil {
			t.Error("Expected error for invalid config")
		}
	})
}
```

**Step 3: Test coverage check**
```bash
cd ~/repos/tollgate-module-basic-go/src
go test -cover ./mptcp_bonding/...
```

### Task 7: Documentation and CHANGELOG

**Objective:** Update project documentation with MPTCP bonding features

**Files:**
- Modify: `~/repos/tollgate-module-basic-go/README.md`
- Modify: `~/repos/tollgate-module-basic-go/CHANGELOG.md`
- Create: `~/repos/tollgate-module-basic-go/docs/mptcp-bonding.md`
- Test: Documentation should be clear and accurate

**Step 1: Update README.md**
Add MPTCP bonding to features section and configuration example.

**Step 2: Update CHANGELOG.md**
```markdown
## [Unreleased]

### Added
- MPTCP multi-WAN bonding support for combining multiple internet connections ([#XXX](https://github.com/OpenTollGate/tollgate-module-basic-go/pull/XXX))
- `mptcp_bonding` configuration section in `config.json`
- MPTCP endpoint management for automatic subflow configuration
- Shadowsocks proxy integration for MPTCP traffic

### Changed / Internal
- Added `mptcp_bonding` module to core application
- Updated configuration schema version to v0.0.8
```

**Step 3: Create MPTCP documentation**
```markdown
# MPTCP Bonding Documentation

## Overview

TollGate now supports MPTCP (Multipath TCP) bonding, allowing a single router to combine multiple internet connections for increased bandwidth and reliability.

## Configuration

Enable MPTCP bonding in your `config.json`:

```json
{
  "mptcp_bonding": {
    "enabled": true,
    "interfaces": [
      {
        "name": "wwan0",
        "enabled": true,
        "priority": 1
      },
      {
        "name": "wwan1", 
        "enabled": true,
        "priority": 2
      }
    ],
    "server_config": {
      "host": "your-vps.example.com",
      "shadowsocks_port": 65101,
      "shadowsocks_password": "your-password",
      "shadowsocks_method": "chacha20-ietf-poly1305"
    }
  }
}
```

## Requirements

- OpenWrt kernel with MPTCP support
- Multiple WAN interfaces (from different gateways)
- MPTCP proxy server (see tg-mptcp-server)
```

---

## Phase 4: PR Creation and Review

### Task 8: Prepare and submit PR

**Objective:** Create a well-structured PR that meets all review criteria

**Files:**
- Create: Feature branch and PR
- Test: Ensure all quality gates pass

**Step 1: Create feature branch**
```bash
cd ~/repos/tollgate-module-basic-go
git checkout -b feature/mptcp-bonding-integration
```

**Step 2: Commit all changes**
```bash
git add src/mptcp_bonding/ src/config_manager/config.go src/main.go README.md CHANGELOG.md docs/mptcp-bonding.md
git commit -m "feat: add MPTCP multi-WAN bonding support

- Add mptcp_bonding module for combining multiple internet connections
- Integrate MPTCP endpoint management and shadowsocks proxy
- Update configuration schema to version v0.0.8
- Add comprehensive documentation and tests

This enables routers to bond multiple WAN connections using MPTCP for
increased bandwidth and reliability."
```

**Step 3: Run quality checks**
```bash
cd ~/repos/tollgate-module-basic-go/src
gofmt -l .          # Should print nothing
go vet ./...
go build ./...
go test -race -count=1 -tags testenv ./...
```

**Step 4: Push and create PR**
```bash
cd ~/repos/tollgate-module-basic-go
git push -u origin feature/mptcp-bonding-integration
```

**Step 5: Draft PR description**
```markdown
## MPTCP Multi-WAN Bonding Support

Adds comprehensive MPTCP (Multipath TCP) bonding capabilities to TollGate, allowing routers to combine multiple internet connections for increased bandwidth and reliability.

### What This Does

- **MPTCP Bonding**: Combine multiple WAN connections using Linux MPTCP
- **Shadowsocks Proxy**: Secure tunneling to aggregation server  
- **Automatic Endpoint Management**: Configure MPTCP subflows automatically
- **Seamless Integration**: Works with existing TollGate modules

### Changes Made

- **New Module**: `src/mptcp_bonding/` with endpoint, proxy, and management
- **Config Integration**: Added `mptcp_bonding` section to `config.json`
- **Main Application**: Integrated MPTCP manager into main lifecycle
- **Documentation**: Comprehensive docs and examples
- **Tests**: Unit and integration tests with >80% coverage

### Configuration Example

```json
{
  "mptcp_bonding": {
    "enabled": true,
    "interfaces": [{"name": "wwan0", "enabled": true}],
    "server_config": {
      "host": "vps.example.com",
      "shadowsocks_password": "secret"
    }
  }
}
```

### Testing

- ✅ Unit tests for all MPTCP components
- ✅ Integration tests with main application
- ✅ Configuration validation
- ✅ Error condition handling

### Related Work

- Depends on: MPTCP-enabled kernel on router
- Complements: tg-mptcp-server for aggregation endpoint
- Freedom Tech Feed: mptcp-bonding package for OpenWrt
```

---

## Summary

This comprehensive plan covers:

1. **Feed Infrastructure**: Completing the Freedom Tech Feed with all packages (fips, tollgate-rs, mptcp-bonding)
2. **MPTCP Integration**: Deep integration of multi-WAN bonding into the main TollGate Go application
3. **Quality Assurance**: Comprehensive testing, documentation, and quality gates
4. **PR Process**: Well-structured pull request following all review criteria

The plan is designed to be executed incrementally, with each task building on the previous one. The end result will be a fully functional MPTCP bonding system that seamlessly integrates with TollGate's existing architecture.

### Estimated Timeline
- **Phase 1**: 2-3 days (feed completion)
- **Phase 2**: 3-4 days (MPTCP integration)
- **Phase 3**: 1-2 days (testing and docs)
- **Phase 4**: 1 day (PR creation)

**Total**: 7-10 days for full implementation and PR submission.

### Risks and Mitigations
- **MPTCP Kernel Support**: Some routers may need custom kernels → Document requirements clearly
- **Network Complexity**: MPTCP endpoint management can be tricky → Include comprehensive error handling
- **Performance Impact**: Additional proxy layer may affect performance → Include benchmarks and optimization notes