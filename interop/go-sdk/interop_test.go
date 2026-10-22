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
	"errors"
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

func ctx(t *testing.T) context.Context {
	c, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	t.Cleanup(cancel)
	return c
}

// =========================================================
// B1: Agent Card Discovery via SDK
// =========================================================

func TestDiscoverAgentCard(t *testing.T) {
	card, err := agentcard.DefaultResolver.Resolve(ctx(t), serverURL())
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

	result, err := client.SendMessage(ctx(t), &a2a.SendMessageRequest{
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
	result, err := client.SendMessage(ctx(t), &a2a.SendMessageRequest{
		Message: a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("create for get")),
	})
	if err != nil {
		t.Fatalf("SendMessage failed: %v", err)
	}
	task := result.(*a2a.Task)

	// Get the task
	got, err := client.GetTask(ctx(t), &a2a.GetTaskRequest{ID: task.ID})
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
	result, err := client.SendMessage(ctx(t), &a2a.SendMessageRequest{
		Message: a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("complete me")),
	})
	if err != nil {
		t.Fatalf("SendMessage failed: %v", err)
	}
	task := result.(*a2a.Task)

	// Cancel should fail — task is already completed
	_, err = client.CancelTask(ctx(t), &a2a.CancelTaskRequest{ID: task.ID})
	if !errors.Is(err, a2a.ErrTaskNotCancelable) {
		t.Fatalf("CancelTask on completed task: want ErrTaskNotCancelable, got %v", err)
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
	_, err := client.SendMessage(ctx(t), &a2a.SendMessageRequest{
		Message: a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("for listing")),
	})
	if err != nil {
		t.Fatalf("SendMessage failed: %v", err)
	}

	// List tasks
	resp, err := client.ListTasks(ctx(t), &a2a.ListTasksRequest{})
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

	_, err := client.GetTask(ctx(t), &a2a.GetTaskRequest{ID: "nonexistent-go-interop"})
	if !errors.Is(err, a2a.ErrTaskNotFound) {
		t.Fatalf("GetTask for nonexistent task: want ErrTaskNotFound, got %v", err)
	}

	t.Logf("Not found error (expected): %v", err)
}

// =========================================================
// B7: SendStreamingMessage via SDK
// =========================================================

func TestSendStreamingMessage(t *testing.T) {
	client := newClient(t)
	defer client.Destroy()

	events := client.SendStreamingMessage(ctx(t), &a2a.SendMessageRequest{
		Message: a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("stream from Go SDK")),
	})

	var collected []a2a.Event
	for event, err := range events {
		if err != nil {
			t.Fatalf("Streaming error: %v", err)
		}
		collected = append(collected, event)
	}

	if len(collected) == 0 {
		t.Fatal("SendStreamingMessage should produce events")
	}

	// Should see status updates progressing to completion
	var sawCompleted bool
	for _, event := range collected {
		if su, ok := event.(*a2a.TaskStatusUpdateEvent); ok {
			t.Logf("Status event: %s", su.Status.State)
			if su.Status.State == a2a.TaskStateCompleted {
				sawCompleted = true
			}
		}
	}

	if !sawCompleted {
		t.Error("Stream should include COMPLETED status event")
	}

	t.Logf("Received %d streaming events", len(collected))
}

// =========================================================
// B8: SubscribeToTask via SDK
// =========================================================

func TestSubscribeToTask(t *testing.T) {
	client := newClient(t)
	defer client.Destroy()

	// First create a task that will complete
	result, err := client.SendMessage(ctx(t), &a2a.SendMessageRequest{
		Message: a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart("for subscribe test")),
	})
	if err != nil {
		t.Fatalf("SendMessage failed: %v", err)
	}

	task, ok := result.(*a2a.Task)
	if !ok {
		t.Fatalf("Expected Task, got %T", result)
	}

	// Subscribe to the completed task: spec §3.1.6 says a terminal task
	// returns UnsupportedOperationError instead of a stream.
	events := client.SubscribeToTask(ctx(t), &a2a.SubscribeToTaskRequest{ID: task.ID})

	for event, err := range events {
		if err == nil {
			t.Fatalf("SubscribeToTask on completed task yielded event %T, want an error", event)
		}
		if !errors.Is(err, a2a.ErrUnsupportedOperation) {
			t.Fatalf("SubscribeToTask on completed task: want ErrUnsupportedOperation, got %v", err)
		}
		t.Logf("Subscribe error (expected for terminal task): %v", err)
		return
	}
	t.Fatal("SubscribeToTask on completed task returned an empty stream, want ErrUnsupportedOperation")
}

// =========================================================
// Helper: create SDK client via discovery
// =========================================================

func newClient(t *testing.T) *a2aclient.Client {
	t.Helper()

	card, err := agentcard.DefaultResolver.Resolve(ctx(t), serverURL())
	if err != nil {
		t.Fatalf("Failed to discover agent card: %v", err)
	}

	client, err := a2aclient.NewFromCard(ctx(t), card)
	if err != nil {
		t.Fatalf("Failed to create client from card: %v", err)
	}

	return client
}
