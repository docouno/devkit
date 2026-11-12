import { useState } from 'react';
import { Button } from '__DEVKIT_UI_PACKAGE__';

export function Playground() {
  const [count, setCount] = useState(0);

  return (
    <main>
      <h1>Design System Playground</h1>
      <p>Edit components in packages/ui/src for instant HMR.</p>
      <div style={{ display: 'flex', gap: '0.75rem', alignItems: 'center' }}>
        <Button
          onClick={() => {
            setCount((value) => value + 1);
          }}
        >
          Increment ({count})
        </Button>
        <Button
          variant="secondary"
          onClick={() => {
            setCount(0);
          }}
        >
          Reset
        </Button>
      </div>
    </main>
  );
}
