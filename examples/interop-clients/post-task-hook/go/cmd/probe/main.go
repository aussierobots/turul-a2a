// Per-agent Go interop probe for `post-task-hook-agent`.
//
// Sends "count 3" three times — each fires the agent's TerminalHook,
// bumping its in-memory success counter — then "metrics" to read the
// counter back. The final artifact should show success >= 3.
//
// Target agent default: http://localhost:3014 (override with A2A_BASE_URL).
package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/a2aproject/a2a-go/v2/a2a"
	"github.com/a2aproject/a2a-go/v2/a2aclient"
	"github.com/a2aproject/a2a-go/v2/a2aclient/agentcard"
)

const (
	defaultAgentBaseURL     = "http://localhost:3014"
	expectedProtocolVersion = "1.0"
)

// Plain-text inputs: the agent's planner parses "count <n>" and the bare
// keyword "metrics". JSON inputs aren't supported by that planner.
var sequence = []string{
	"count 3",
	"count 3",
	"count 3",
	"metrics",
}

func agentBaseURL() string {
	if v := os.Getenv("A2A_BASE_URL"); v != "" {
		return v
	}
	return defaultAgentBaseURL
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	if string(a2a.Version) != expectedProtocolVersion {
		return fmt.Errorf(
			"a2a-go SDK protocol version drift: got %q, expected %q",
			string(a2a.Version), expectedProtocolVersion,
		)
	}
	fmt.Printf("a2a-go protocol version: %s\n", a2a.Version)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	baseURL := agentBaseURL()
	fmt.Printf("target: %s\n", baseURL)

	card, err := agentcard.DefaultResolver.Resolve(ctx, baseURL)
	if err != nil {
		return fmt.Errorf("resolve agent card from %s: %w", baseURL, err)
	}
	fmt.Println("--- AgentCard ---")
	fmt.Printf("Name:    %s\n", card.Name)
	for _, iface := range card.SupportedInterfaces {
		fmt.Printf("  - %s @ %s\n", iface.ProtocolBinding, iface.URL)
	}

	client, err := a2aclient.NewFromCard(ctx, card)
	if err != nil {
		return fmt.Errorf("build client from card: %w", err)
	}
	defer client.Destroy()

	for i, text := range sequence {
		if err := sendOne(ctx, client, i+1, text); err != nil {
			return fmt.Errorf("step %d %q: %w", i+1, text, err)
		}
	}
	return nil
}

func sendOne(ctx context.Context, client *a2aclient.Client, step int, text string) error {
	msg := a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart(text))
	fmt.Printf("--- SendMessage step %d ---\n", step)
	fmt.Printf("text: %q\n", text)

	result, err := client.SendMessage(ctx, &a2a.SendMessageRequest{Message: msg})
	if err != nil {
		return fmt.Errorf("send: %w", err)
	}
	task, ok := result.(*a2a.Task)
	if !ok {
		return fmt.Errorf("expected *a2a.Task, got %T", result)
	}
	fmt.Printf("state=%s\n", task.Status.State)
	for i, art := range task.Artifacts {
		fmt.Printf("  artifact[%d] name=%q parts=%d\n", i, art.Name, len(art.Parts))
		for j, p := range art.Parts {
			fmt.Printf("    part[%d].text=%s\n", j, p.Text())
		}
	}
	return nil
}
