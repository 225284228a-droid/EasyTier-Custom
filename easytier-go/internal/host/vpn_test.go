package host

import (
	"context"
	"strings"
	"testing"
	"time"

	apiinstance "github.com/easytier/easytier/easytier-go/proto/api/instance"
)

func TestEmbeddedSubnetExitConfigAndStatus(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	host, err := New(ctx, Options{})
	if err != nil {
		t.Fatalf("create host: %v", err)
	}
	defer host.Close(ctx)
	base, err := encodeInstanceConfig(managementTestConfig(t, "subnet-exit"))
	if err != nil {
		t.Fatalf("encode config: %v", err)
	}
	instance, err := host.CreateInstanceTOML(ctx, "subnet-exit", "", base+`
[vpn]
[[vpn.rules]]
cidr = "203.0.113.0/24"
exit = "10.144.0.2"
[[vpn.rules]]
cidr = "198.51.100.0/24"
exit = "10.144.0.3"
[[vpn.rules]]
cidr = "2001:db8::/32"
exit = "fd42::2"
`)
	if err != nil {
		t.Fatalf("create subnet exit instance: %v", err)
	}
	status := new(apiinstance.GetVpnStatusResponse)
	if err := instance.callRPCRequest(ctx, "api.instance.PeerManageRpc.GetVpnStatus",
		new(apiinstance.GetVpnStatusRequest), status); err != nil {
		t.Fatalf("get subnet exit status: %v", err)
	}
	if !status.Enabled || status.ExitEnabled || status.PolicyGeneration == 0 {
		t.Fatalf("unexpected subnet exit status: %v", status)
	}
	if len(status.Rules) != 3 {
		t.Fatalf("status rules = %v, want three single-exit rules", status.Rules)
	}
	for i, want := range [][2]string{
		{"203.0.113.0/24", "10.144.0.2"},
		{"198.51.100.0/24", "10.144.0.3"},
		{"2001:db8::/32", "fd42::2"},
	} {
		rule := status.Rules[i]
		if rule.Cidr != want[0] || rule.ExitAddr != want[1] || rule.State != "down" {
			t.Fatalf("rule %d = %v, want %v with unreachable exit", i, rule, want)
		}
		if !strings.Contains("\n"+strings.Join(status.CaptureRoutes, "\n")+"\n", "\n"+want[0]+"\n") {
			t.Fatalf("capture routes %v omit %s", status.CaptureRoutes, want[0])
		}
	}
	if err := instance.Close(ctx); err != nil {
		t.Fatalf("close subnet exit instance: %v", err)
	}

	for _, test := range []struct {
		name, config, errorText string
	}{
		{"legacy option", "[vpn]\ndns_protect = true\n", "dns_protect"},
		{"legacy rule", "[vpn]\n[[vpn.rules]]\ncidr = '203.0.113.0/24'\naction = 'exit'\nexits = ['10.144.0.2']\n", "unknown field"},
		{"duplicate subnet", "[vpn]\n[[vpn.rules]]\ncidr = '203.0.113.0/24'\nexit = '10.144.0.2'\n[[vpn.rules]]\ncidr = '203.0.113.0/24'\nexit = '10.144.0.3'\n", "duplicate VPN rule CIDR"},
		{"invalid exit", "[vpn]\n[[vpn.rules]]\ncidr = '203.0.113.0/24'\nexit = '127.0.0.1'\n", "invalid exit address"},
	} {
		t.Run(test.name, func(t *testing.T) {
			unexpected, err := host.CreateInstanceTOML(ctx, "invalid-subnet-exit", "", base+"\n"+test.config)
			if unexpected != nil {
				_ = unexpected.Close(ctx)
			}
			if err == nil || !strings.Contains(err.Error(), test.errorText) {
				t.Fatalf("create invalid instance error = %v, want %q", err, test.errorText)
			}
		})
	}
}
