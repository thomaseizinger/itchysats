import { ChakraProvider } from "@chakra-ui/react";
import React, { ReactElement } from "react";
import ReactDOM from "react-dom";
import { SSEProvider } from "react-hooks-sse";
import { BrowserRouter } from "react-router-dom";
import App from "./App";
import "./index.css";
import reportWebVitals from "./reportWebVitals";
import theme from "./theme";

ReactDOM.render(
    <React.StrictMode>
        <ChakraProvider theme={theme}>
            <FeedProvider>
                <BrowserRouter>
                    <App />
                </BrowserRouter>
            </FeedProvider>
        </ChakraProvider>
    </React.StrictMode>,
    document.getElementById("root"),
);

interface FeedProviderProps {
    children: ReactElement;
}

export function FeedProvider({ children }: FeedProviderProps) {
    return <SSEProvider endpoint="http://localhost:8000/feed">{children}</SSEProvider>;
}
