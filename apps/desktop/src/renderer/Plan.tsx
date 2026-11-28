// The model's plan for the running turn, above the prompt box.
import type { ControlsState } from './controls';

/** The model's plan for the running turn, as Claude Code's checklist; Ctrl+T hides or shows it. */
export function PlanPanel({ controls, shown }: { controls: ControlsState; shown: boolean }) {
  const left = controls.plan.filter((step) => step.status !== 'completed').length;
  if (!controls.plan.length || left === 0) return null;
  if (!shown) {
    return (
      <div className="plan meta" data-testid="plan">
        Plan: {left} step{left === 1 ? '' : 's'} left (Ctrl+T to show)
      </div>
    );
  }
  return (
    <ol className="plan" data-testid="plan" aria-label="Plan (Ctrl+T to hide)">
      {controls.plan.map((step, i) => (
        <li key={i} className={`plan-step ${step.status}`}>
          <span aria-hidden>{step.status === 'completed' ? '✓' : step.status === 'inProgress' ? '◐' : '○'}</span> {step.step}
        </li>
      ))}
    </ol>
  );
}
