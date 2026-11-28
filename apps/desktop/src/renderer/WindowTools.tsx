// The buttons at the window's top left, beside the system's own on a Mac, as
// in Claude Code's desktop app: hide or show the sidebar (Cmd+B), and Back
// and Forward (Cmd+[ and Cmd+]) through the sessions the window has shown.
import { BackIcon, ForwardIcon, SidebarIcon } from './Icons';

interface Props {
  sidebarShown: boolean;
  onSidebar(): void;
  canGoBack: boolean;
  canGoForward: boolean;
  onBack(): void;
  onForward(): void;
}

export function WindowTools({ sidebarShown, onSidebar, canGoBack, canGoForward, onBack, onForward }: Props) {
  const sidebar = sidebarShown ? 'Hide sidebar' : 'Show sidebar';
  return (
    <div className="window-tools" role="toolbar" aria-label="Window">
      <button className="icon-button" aria-label={sidebar} title={`${sidebar} (⌘B)`} onClick={onSidebar}>
        <SidebarIcon />
      </button>
      <button className="icon-button" aria-label="Back" title="Back (⌘[)" disabled={!canGoBack} onClick={onBack}>
        <BackIcon />
      </button>
      <button className="icon-button" aria-label="Forward" title="Forward (⌘])" disabled={!canGoForward} onClick={onForward}>
        <ForwardIcon />
      </button>
    </div>
  );
}
