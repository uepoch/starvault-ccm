import { MantineProvider, createTheme } from "@mantine/core";
import "@mantine/core/styles.css";
import Library from "./Library";

const theme = createTheme({
  primaryColor: "blue",
  defaultRadius: "sm",
});

export default function App() {
  return (
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <Library />
    </MantineProvider>
  );
}
