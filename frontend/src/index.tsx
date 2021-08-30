import { ChakraProvider } from "@chakra-ui/react";
import React, { ReactElement } from "react";
import ReactDOM from "react-dom";
import { BrowserRouter } from "react-router-dom";
import { EventSourceProvider } from "react-sse-hooks";
import App from "./App";
import "./index.css";
import reportWebVitals from "./reportWebVitals";
import theme from "./theme";

ReactDOM.render(
    <React.StrictMode>
        <ChakraProvider theme={theme}>
            <EventSourceProvider>
                <BrowserRouter>
                    <App />
                </BrowserRouter>
            </EventSourceProvider>
        </ChakraProvider>
    </React.StrictMode>,
    document.getElementById("root"),
);
