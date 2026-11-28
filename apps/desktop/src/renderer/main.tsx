import { createRoot } from 'react-dom/client';
import './styles.css';
import { App } from './App';
import { applyPrefs, loadPrefs } from './prefs';

// Before the first paint, so a chosen theme doesn't flash the other one.
applyPrefs(document.documentElement, loadPrefs(window.localStorage));

createRoot(document.getElementById('root')!).render(<App />);
