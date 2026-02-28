import { useState } from "react";
import { getBrainUrl, getApiKey } from "../../api/config";
import { resetMcpClient } from "../../api/mcp";

interface Props {
  onClose: () => void;
}

export default function SettingsModal({ onClose }: Props) {
  const [brainUrl, setBrainUrl] = useState(getBrainUrl);
  const [apiKey,   setApiKey]   = useState(getApiKey);
  const [saved,    setSaved]    = useState(false);

  const save = () => {
    localStorage.setItem("brain_url", brainUrl);
    localStorage.setItem("api_key",   apiKey);
    resetMcpClient(); // force reconnect on next tool call with new credentials
    setSaved(true);
    setTimeout(() => setSaved(false), 2000);
  };

  const handleBackdrop = (e: React.MouseEvent) => {
    if (e.target === e.currentTarget) onClose();
  };

  return (
    <div className="modal-backdrop" onClick={handleBackdrop}>
      <div className="modal">
        <div className="modal-header">
          ⚙ Settings
          <button className="close-btn" onClick={onClose}>×</button>
        </div>

        <div className="modal-body">
          <label htmlFor="brain-url">Brain URL</label>
          <input
            id="brain-url"
            value={brainUrl}
            onChange={(e) => setBrainUrl(e.target.value)}
            placeholder="http://localhost:3001"
            spellCheck={false}
          />
          <label htmlFor="api-key">API Key</label>
          <input
            id="api-key"
            type="password"
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
            placeholder="openclaw"
            spellCheck={false}
          />
          <div style={{ fontSize: 10, color: "var(--text-muted)", lineHeight: 1.5 }}>
            Settings are stored in your browser's localStorage.<br />
            Saving reconnects the MCP client immediately.
          </div>
        </div>

        <div className="modal-footer">
          {saved && <span className="saved-msg">Saved ✓</span>}
          <button className="btn" onClick={save}>Save &amp; Reconnect</button>
        </div>
      </div>
    </div>
  );
}
