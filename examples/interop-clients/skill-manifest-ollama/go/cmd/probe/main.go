// Per-agent Go interop probe for `skill-manifest-ollama-agent`.
//
// Sends one JSON payload to the manifest-driven `greet` skill and prints
// the resulting artifact. See README.md for full context.
//
// Target agent default: http://localhost:3010 (override with A2A_BASE_URL).
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
	defaultAgentBaseURL     = "http://localhost:3010"
	expectedProtocolVersion = "1.0"

	// greetPayload is the JSON the manifest's `greet` skill expects.
	// Matches the SKILL.md inputSchema: {user:{name:string}, style:enum}.
	greetPayload = `{"user":{"name":"Ada"},"style":"formal"}`
)

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

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	baseURL := agentBaseURL()
	fmt.Printf("target: %s\n", baseURL)

	card, err := agentcard.DefaultResolver.Resolve(ctx, baseURL)
	if err != nil {
		return fmt.Errorf("resolve agent card from %s: %w", baseURL, err)
	}
	fmt.Println("--- AgentCard ---")
	fmt.Printf("Name:    %s\n", card.Name)
	fmt.Printf("Version: %s\n", card.Version)
	for _, iface := range card.SupportedInterfaces {
		fmt.Printf("  - %s @ %s\n", iface.ProtocolBinding, iface.URL)
	}

	client, err := a2aclient.NewFromCard(ctx, card)
	if err != nil {
		return fmt.Errorf("build client from card: %w", err)
	}
	defer client.Destroy()

	msg := a2a.NewMessage(
		a2a.MessageRoleUser,
		a2a.NewTextPart(greetPayload),
	)
	fmt.Println("--- SendMessage request ---")
	fmt.Printf("payload: %s\n", greetPayload)

	result, err := client.SendMessage(ctx, &a2a.SendMessageRequest{Message: msg})
	if err != nil {
		return fmt.Errorf("send message: %w", err)
	}

	fmt.Println("--- SendMessage response ---")
	task, ok := result.(*a2a.Task)
	if !ok {
		return fmt.Errorf("expected *a2a.Task, got %T", result)
	}
	fmt.Printf("kind=Task id=%s state=%s\n", task.ID, task.Status.State)
	for i, art := range task.Artifacts {
		fmt.Printf("  artifact[%d] id=%s name=%q parts=%d\n",
			i, art.ID, art.Name, len(art.Parts))
		for j, p := range art.Parts {
			fmt.Printf("    part[%d].text=%s\n", j, p.Text())
		}
	}
	return nil
}
