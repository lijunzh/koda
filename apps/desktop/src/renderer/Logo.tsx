// Koda's logo, the app's own icon (assets/icon.svg), wherever the app
// shows who it is: while it starts, on an empty session, in Settings.
import icon from '../../assets/icon.svg';

/** The logo as a picture beside the name it stands for, so it needs no text of its own. */
export function Logo({ size }: { size: number }) {
  return <img className="logo" data-testid="logo" src={icon} width={size} height={size} alt="" draggable={false} />;
}
