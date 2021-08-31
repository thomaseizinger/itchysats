import {
    Box,
    Button,
    Center,
    Flex,
    HStack,
    NumberDecrementStepper,
    NumberIncrementStepper,
    NumberInput,
    NumberInputField,
    NumberInputStepper,
    SimpleGrid,
    StackDivider,
    Text,
    useToast,
    VStack,
} from "@chakra-ui/react";
import axios from "axios";
import React, { useState } from "react";
import { useAsync } from "react-async";
import { Link as RouteLink, Route, Switch } from "react-router-dom";
import { useEventSource, useEventSourceListener } from "react-sse-hooks";
import "./App.css";
import CFD from "./components/CFD";

/* TODO: Change from localhost:8000 */
const BASE_URL = "http://localhost:8000";

function useLatestEvent<T>(source: EventSource, event_name: string): T | null {
    const [state, setState] = useState<T | null>(null);

    useEventSourceListener<T | null>(
        {
            source: source,
            startOnInit: true,
            event: {
                name: event_name,
                listener: ({ data }) => setState(data),
            },
        },
        [source],
    );

    return state;
}

interface Offer {
    id: string;
    price: number;
    min_amount: number;
    max_amount: number;
    leverage: number;
    trading_pair: string;
    liquidation_price: number;
}

interface Cfd {
    initial_price: number;

    leverage: number;
    trading_pair: string;
    liquidation_price: number;
    quantity_btc: number;
    quantity_usd: number;
    profit_btc: number;
    profit_usd: number;
    creation_date: string;
    state: string;
}

interface CfdTakeRequestPayload {
    offer_id: string;
    quantity: number;
}

async function postCfdTakeRequest(payload: CfdTakeRequestPayload) {
    let res = await axios.post(BASE_URL + `/cfd`, JSON.stringify(payload));

    if (res.status !== 200) {
        throw new Error("failed to create new swap");
    }
}

function App() {
    let source = useEventSource({ source: BASE_URL + "/feed" });

    const cfds = useLatestEvent<Cfd[]>(source, "cfds");
    const offer = useLatestEvent<Offer>(source, "offer");
    const balance = useLatestEvent<number>(source, "balance");

    const toast = useToast();
    let [quantity, setQuantity] = useState<string>("10000");
    const format = (val: any) => `$` + val;
    const parse = (val: any) => val.replace(/^\$/, "");

    let { run: makeNewTakeRequest, isLoading: isCreatingNewTakeRequest } = useAsync({
        deferFn: async ([payload]: any[]) => {
            try {
                await postCfdTakeRequest(payload as CfdTakeRequestPayload);
            } catch (e) {
                const description = typeof e === "string" ? e : JSON.stringify(e);

                toast({
                    title: "Error",
                    description,
                    status: "error",
                    duration: 9000,
                    isClosable: true,
                });
            }
        },
    });

    return (
        <Center marginTop={50}>
            <HStack>
                <Box marginRight={5}>
                    <VStack align={"top"}>
                        <NavLink text={"trade"} path={"/trade"} />
                        <NavLink text={"wallet"} path={"/wallet"} />
                        <NavLink text={"settings"} path={"/settings"} />
                    </VStack>
                </Box>
                <Box width={1200} height={600}>
                    <Switch>
                        <Route path="/trade">
                            <Flex direction={"row"} height={"100%"}>
                                <Flex direction={"row"} width={"100%"}>
                                    <VStack
                                        spacing={5}
                                        shadow={"md"}
                                        padding={5}
                                        width={"100%"}
                                        divider={<StackDivider borderColor="gray.200" />}
                                    >
                                        <Box width={"100%"} overflow={"scroll"}>
                                            <SimpleGrid columns={2} spacing={10}>
                                                {cfds && cfds.map((cfd, index) =>
                                                    <CFD
                                                        key={"cfd_" + index}
                                                        number={index}
                                                        liquidation_price={cfd.liquidation_price}
                                                        amount={cfd.quantity_usd}
                                                        profit={cfd.profit_usd}
                                                        creation_date={cfd.creation_date}
                                                        status={cfd.state}
                                                    />
                                                )}
                                            </SimpleGrid>
                                        </Box>
                                    </VStack>
                                </Flex>
                                <Flex width={"50%"} marginLeft={5}>
                                    <VStack spacing={5} shadow={"md"} padding={5} align={"stretch"}>
                                        <HStack>
                                            <Text align={"left"}>Your balance:</Text>
                                            <Text>{balance}</Text>
                                        </HStack>
                                        <HStack>
                                            <Text align={"left"}>Current Price:</Text>
                                            <Text>{offer?.price}</Text>
                                        </HStack>
                                        <HStack>
                                            <Text>Quantity:</Text>
                                            <NumberInput
                                                onChange={(valueString: string) => setQuantity(parse(valueString))}
                                                value={format(quantity)}
                                            >
                                                <NumberInputField />
                                                <NumberInputStepper>
                                                    <NumberIncrementStepper />
                                                    <NumberDecrementStepper />
                                                </NumberInputStepper>
                                            </NumberInput>
                                        </HStack>
                                        <Text>Leverage:</Text>
                                        {/* TODO: consider button group */}
                                        <Flex justifyContent={"space-between"}>
                                            <Button disabled={true}>x1</Button>
                                            <Button disabled={true}>x2</Button>
                                            <Button colorScheme="blue" variant="solid">x{offer?.leverage}</Button>
                                        </Flex>
                                        {<Button
                                            disabled={isCreatingNewTakeRequest}
                                            variant={"solid"}
                                            colorScheme={"blue"}
                                            onClick={() => {
                                                let payload: CfdTakeRequestPayload = {
                                                    offer_id: offer!.id,
                                                    quantity: Number.parseFloat(quantity),
                                                };
                                                makeNewTakeRequest(payload);
                                            }}
                                        >
                                            BUY
                                        </Button>}
                                    </VStack>
                                </Flex>
                            </Flex>
                        </Route>
                        <Route path="/wallet">
                            <Center height={"100%"} shadow={"md"}>
                                <Box>
                                    <Text>Wallet</Text>
                                </Box>
                            </Center>
                        </Route>
                        <Route path="/settings">
                            <Center height={"100%"} shadow={"md"}>
                                <Box>
                                    <Text>Settings</Text>
                                </Box>
                            </Center>
                        </Route>
                    </Switch>
                </Box>
            </HStack>
        </Center>
    );
}

type NavLinkProps = { text: string; path: string };

const NavLink = ({ text, path }: NavLinkProps) => (
    <RouteLink to={path}>
        <Route
            path={path}
            children={({ match }) => (
                <Button width="100px" colorScheme="blue" variant={match?.path ? "solid" : "outline"}>
                    {text}
                </Button>
            )}
        />
    </RouteLink>
);

export default App;
