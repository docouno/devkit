import type { ButtonHTMLAttributes, ReactElement, ReactNode } from 'react';
import './styles.css';

export type ButtonVariant = 'primary' | 'secondary';

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  readonly variant?: ButtonVariant;
  readonly children?: ReactNode;
}

export function Button({
  variant = 'primary',
  className,
  type = 'button',
  children,
  ...rest
}: ButtonProps): ReactElement {
  const classes = className ? `devkit-ui-button ${className}` : 'devkit-ui-button';
  return (
    <button {...rest} type={type} data-variant={variant} className={classes}>
      {children}
    </button>
  );
}
