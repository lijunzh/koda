// The app's icons: small line drawings of its own, at the size Claude Code's
// desktop app draws its sidebar and title-bar icons.
import type { ReactNode } from 'react';

function Icon({ children, size = 16 }: { children: ReactNode; size?: number }) {
  return (
    <svg
      className="icon"
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      {children}
    </svg>
  );
}

/** A padlock, for a confidential session: a body with a shackle over it. */
export const LockIcon = ({ size }: { size?: number }) => (
  <Icon size={size}>
    <rect x="3.5" y="7.2" width="9" height="6.3" rx="1.5" />
    <path d="M5.6 7.2V5.4a2.4 2.4 0 0 1 4.8 0v1.8" />
  </Icon>
);
/** The lock a confidential session carries in a list and beside its title, with what it means. */
export const ConfidentialMark = ({ tip }: { tip: string }) => (
  <span className="lock" role="img" aria-label="Confidential" title={tip} data-testid="confidential">
    <LockIcon size={12} />
  </span>
);
export const PlusIcon = () => (
  <Icon>
    <path d="M8 3v10M3 8h10" />
  </Icon>
);
export const SearchIcon = () => (
  <Icon>
    <circle cx="7" cy="7" r="4.2" />
    <path d="M10.2 10.2 13.5 13.5" />
  </Icon>
);
export const GearIcon = () => (
  <Icon>
    <circle cx="8" cy="8" r="2.1" />
    <path d="M8 1.8v1.7M8 12.5v1.7M1.8 8h1.7M12.5 8h1.7M3.6 3.6l1.2 1.2M11.2 11.2l1.2 1.2M12.4 3.6l-1.2 1.2M4.8 11.2l-1.2 1.2" />
  </Icon>
);
/** The side panel: a window with a column at its right. */
export const PanelIcon = () => (
  <Icon>
    <rect x="2" y="3" width="12" height="10" rx="2" />
    <path d="M10 3v10" />
  </Icon>
);
/** The activity panel: a list. */
export const ListIcon = () => (
  <Icon>
    <path d="M6 4.5h7.5M6 8h7.5M6 11.5h7.5" />
    <path d="M2.6 4.5h.3M2.6 8h.3M2.6 11.5h.3" strokeWidth="1.8" />
  </Icon>
);
export const CloseIcon = () => (
  <Icon>
    <path d="m4 4 8 8M12 4l-8 8" />
  </Icon>
);
export const ChangesIcon = () => (
  <Icon>
    <rect x="2.5" y="2.5" width="11" height="11" rx="2" />
    <path d="M8 5v3.4M6.3 6.7h3.4M6.3 10.8h3.4" />
  </Icon>
);
export const TasksIcon = () => (
  <Icon>
    <path d="m5.2 5-3 3 3 3M10.8 5l3 3-3 3" />
    <path d="m8 6.2 1.8 1.8L8 9.8 6.2 8z" />
  </Icon>
);
export const FolderIcon = () => (
  <Icon>
    <path d="M2 5.2c0-.9.7-1.6 1.6-1.6h2.5l1.5 1.6h4.8c.9 0 1.6.7 1.6 1.6v4.4c0 .9-.7 1.6-1.6 1.6H3.6c-.9 0-1.6-.7-1.6-1.6z" />
  </Icon>
);
export const ArchiveIcon = () => (
  <Icon>
    <rect x="2" y="3" width="12" height="3" rx="1" />
    <path d="M3.2 6v5.4c0 .9.7 1.6 1.6 1.6h6.4c.9 0 1.6-.7 1.6-1.6V6M6.5 9h3" />
  </Icon>
);
export const SidebarIcon = () => (
  <Icon>
    <rect x="2" y="3" width="12" height="10" rx="2" />
    <path d="M6 3v10" />
  </Icon>
);
export const BackIcon = () => (
  <Icon>
    <path d="M9.8 3.8 5.6 8l4.2 4.2" />
  </Icon>
);
export const ForwardIcon = () => (
  <Icon>
    <path d="m6.2 3.8 4.2 4.2-4.2 4.2" />
  </Icon>
);
