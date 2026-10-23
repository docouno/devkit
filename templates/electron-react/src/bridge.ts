export interface Greeting {
  message: string;
}
export interface DesktopCapabilities {
  apiEnabled: boolean;
}
export interface DesktopBridge {
  getCapabilities(): Promise<DesktopCapabilities>;
  greet(name: string): Promise<Greeting>;
}
