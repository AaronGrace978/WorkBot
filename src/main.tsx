import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";

type ErrorBoundaryState = {
  error: Error | null;
};

class RootErrorBoundary extends React.Component<
  { children: React.ReactNode },
  ErrorBoundaryState
> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  render() {
    if (this.state.error) {
      return (
        <div
          style={{
            display: "grid",
            placeItems: "center",
            height: "100vh",
            padding: 32,
            background: "#f8fafd",
            color: "#202124",
            fontFamily: '"Google Sans", "Segoe UI", sans-serif',
          }}
        >
          <div
            style={{
              maxWidth: 520,
              padding: 24,
              background: "#fff",
              border: "1px solid #dfe3eb",
              borderRadius: 20,
              boxShadow: "0 8px 28px rgba(60, 64, 67, 0.1)",
            }}
          >
            <h2 style={{ margin: "0 0 8px", fontSize: 20 }}>
              Work Bot recovered from a UI error
            </h2>
            <p style={{ margin: "0 0 14px", fontSize: 13, color: "#5f6368" }}>
              {this.state.error.message || String(this.state.error)}
            </p>
            <div style={{ display: "flex", gap: 10 }}>
              <button
                type="button"
                className="primary"
                onClick={() => this.setState({ error: null })}
              >
                Resume UI
              </button>
              <button
                type="button"
                className="ghost"
                onClick={() => {
                  try {
                    localStorage.removeItem("gemma-work-bot-transcript");
                  } catch {
                    // ignore
                  }
                  window.location.reload();
                }}
              >
                Reset Chat &amp; Reload
              </button>
            </div>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <RootErrorBoundary>
      <App />
    </RootErrorBoundary>
  </React.StrictMode>,
);
