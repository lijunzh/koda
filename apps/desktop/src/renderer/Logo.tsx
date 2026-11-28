// Koda's logo wherever the app shows who it is: while it starts, on an empty
// session, in Settings. It is the mark (assets/mark.svg): the icon's paw print
// in amber, without the tile, on the page's own background in light and dark.
import mark from '../../assets/mark.svg';

/** The logo as a picture beside the name it stands for, so it needs no text of its own. */
export function Logo({ size }: { size: number }) {
  return <img className="logo" data-testid="logo" src={mark} width={size} height={size} alt="" draggable={false} />;
}
