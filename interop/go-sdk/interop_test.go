// Package interop tests the official Go A2A SDK client against the turul-a2a Rust server.
//
// The Go SDK (a2aproject/a2a-go v2) implements A2A spec v1.0, matching our proto.
// This is the primary external interoperability test suite.
//
// Prerequisites:
//
//	Running turul-a2a server (cargo run -p echo-agent)
//	TURUL_SERVER_URL env var (default: http://localhost:3000)
//
// Run:
//
//	cd interop/go-sdk && go test -v
package interop

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/a2aproject/a2a-go/v2/a2a"
	"github.com/a2aproject/a2a-go/v2/a2aclient"
	"github.com/a2aproject/a2a-go/v2/a2aclient/agentcard"
)

func serverURL() string {
	if url := os.Getenv("TURUL_SERVER_URL"); url != "" {
		return url
	}
	return "http://localhost:3000"
}

func ctx() context.Context {
	c, _ := context.WithTimeout(context.Background(), 10*time.Second)
	return c
}

// =========================================================
// B1: Agent Card Discovery via SDK
// =========================================================

func TestDiscoverAgentCard(t *testing.T) {
	card, err := agentcard.DefaultResolver.Resolve(ctx(), serverURL())
	if err != nil {
		t.Fatalf("Failed to discover agent card: %v", err)
	}

	if card.Name == "" {
		t.Error("Agent card name should not be empty")
	}
	if len(card.SupportedInterfaces) == 0 {
		t.Error("Agent card should have at least one supported interface")
	}
	if card.Description == "" {
		t.Error("Agent card description should not be empty")
	}

	t.Logf("Discovered agent: %s (v%s)", card.Name, card.Version)
}

// =========================================================
// B2: Send Message via SDK Client
// =========================================================

func TestSendMessage(t *testing.T) {
	client := newClient(t)
	defer client.Destroy()

	result, err := client.SendMessage(ctx(), &a2a.SendMessageRequest{
		Message: a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("hello from Go SDK")),
	})
	if err != nil {
		t.Fatalf("SendMessage failed: %v", err)
	}

	// Result is either *Task or *Message
	task, ok := result.(*a2a.Task)
	if !ok {
		t.Fatalf("Expected *Task result, got %T", result)
	}
	if task.ID == "" {
		t.Error("Task ID should not be empty")
	}

	t.Logf("Task %s status: %s", task.ID, task.Status.State)
}

// =========================================================
// B3: Get Task via SDK Client
// =========================================================

func TestGetTask(t *testing.T) {
	client := newClient(t)
	defer client.Destroy()

	// Create a task first
	result, err := client.SendMessage(ctx(), &a2a.SendMessageRequest{
		Message: a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("create for get")),
	})
	if err != nil {
		t.Fatalf("SendMessage failed: %v", err)
	}
	task := result.(*a2a.Task)

	// Get the task
	got, err := client.GetTask(ctx(), &a2a.GetTaskRequest{ID: task.ID})
	if err != nil {
		t.Fatalf("GetTask failed: %v", err)
	}

	if got.ID != task.ID {
		t.Errorf("Expected task ID %s, got %s", task.ID, got.ID)
	}

	t.Logf("Got task %s: status=%s", got.ID, got.Status.State)
}

// =========================================================
// B4: Cancel Completed Task (Error Case)
// =========================================================

func TestCancelCompletedTask(t *testing.T) {
	client := newClient(t)
	defer client.Destroy()

	// Create and complete a task
	result, err := client.SendMessage(ctx(), &a2a.SendMessageRequest{
		Message: a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("complete me")),
	})
	if err != nil {
		t.Fatalf("SendMessage failed: %v", err)
	}
	task := result.(*a2a.Task)

	// Cancel should fail — task is already completed
	_, err = client.CancelTask(ctx(), &a2a.CancelTaskRequest{ID: task.ID})
	if err == nil {
		t.Fatal("CancelTask on completed task should return error")
	}

	t.Logf("Cancel error (expected): %v", err)
}

// =========================================================
// B5: List Tasks via SDK Client
// =========================================================

func TestListTasks(t *testing.T) {
	client := newClient(t)
	defer client.Destroy()

	// Create a task first
	_, err := client.SendMessage(ctx(), &a2a.SendMessageRequest{
		Message: a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("for listing")),
	})
	if err != nil {
		t.Fatalf("SendMessage failed: %v", err)
	}

	// List tasks
	resp, err := client.ListTasks(ctx(), &a2a.ListTasksRequest{})
	if err != nil {
		t.Fatalf("ListTasks failed: %v", err)
	}

	if len(resp.Tasks) == 0 {
		t.Error("Expected at least one task in list")
	}

	t.Logf("Listed %d tasks (total: %d)", len(resp.Tasks), resp.TotalSize)
}

// =========================================================
// B6: Get Task Not Found
// =========================================================

func TestGetTaskNotFound(t *testing.T) {
	client := newClient(t)
	defer client.Destroy()

	_, err := client.GetTask(ctx(), &a2a.GetTaskRequest{ID: "nonexistent-go-interop"})
	if err == nil {
		t.Fatal("GetTask for nonexistent task should return error")
	}

	t.Logf("Not found error (expected): %v", err)
}

// =========================================================
// Helper: create SDK client via discovery
// =========================================================

func newClient(t *testing.T) *a2aclient.Client {
	t.Helper()

	card, err := agentcard.DefaultResolver.Resolve(ctx(), serverURL())
	if err != nil {
		t.Fatalf("Failed to discover agent card: %v", err)
	}

	client, err := a2aclient.NewFromCard(ctx(), card)
	if err != nil {
		t.Fatalf("Failed to create client from card: %v", err)
	}

	return client
}
