import { useState, useEffect, useCallback } from "react";

const REPO_URL = "https://api.github.com/repos/itsRevela/Revelations-Launcher/releases/latest";
const LAST_SEEN_KEY = "lce-launcher-last-seen-release";

export function useUpdateCheck() {
  const [updateMessage, setUpdateMessage] = useState<string | null>(null);

  const checkUpdates = useCallback(async () => {
    try {
      const response = await fetch(REPO_URL);
      if (!response.ok) return;

      const data = await response.json();
      const publishedAt = data.published_at;
      if (!publishedAt) return;

      const lastSeen = localStorage.getItem(LAST_SEEN_KEY);
      if (!lastSeen) {
        // First launch: store current release as seen
        localStorage.setItem(LAST_SEEN_KEY, publishedAt);
        return;
      }

      if (publishedAt > lastSeen) {
        setUpdateMessage("A new launcher update is available!");
      }
    } catch (e) {
      console.error("Failed to check for updates:", e);
    }
  }, []);

  const clearUpdateMessage = useCallback(() => {
    setUpdateMessage(null);
    // Mark as seen when dismissed
    fetch(REPO_URL)
      .then(r => r.json())
      .then(data => {
        if (data.published_at) {
          localStorage.setItem(LAST_SEEN_KEY, data.published_at);
        }
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    checkUpdates();
  }, [checkUpdates]);

  return {
    updateMessage,
    clearUpdateMessage,
  };
}
