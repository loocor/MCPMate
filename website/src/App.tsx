import { useEffect, useState } from "react";
import { BrowserRouter, Route, Routes, useLocation } from "react-router-dom";
import { LanguageProvider } from "./components/LanguageProvider";
import Footer from "./components/layout/Footer";
import Navbar from "./components/layout/Navbar";
import { ThemeProvider } from "./components/ThemeProvider";
import Homepage from "./pages/Homepage";
import Privacy from "./pages/Privacy";
import Terms from "./pages/Terms";
import { initGA, trackPageView } from "./utils/analytics";
import DomainMigrationBanner from "./components/ui/DomainMigrationBanner";
import { renderDocRoutes } from "./docs/DocRoutes";
import HomepageConcept from "./pages/HomepageConcept";

function getShellClass(pathname: string): string {
	if (pathname === "/") {
		return "marketing-shell min-h-screen flex flex-col";
	}

	if (pathname.startsWith("/docs/")) {
		return "docs-shell min-h-screen flex flex-col";
	}

	return "min-h-screen flex flex-col bg-slate-50 text-slate-900 dark:bg-brand-bg dark:text-brand-foreground";
}

// page view tracking component
function Analytics() {
	const location = useLocation();

	useEffect(() => {
		trackPageView(location.pathname + location.search);
	}, [location]);

	return null;
}

// Ensure top-of-page on route changes for long-form documents
function ScrollTopOnDocs() {
	const { pathname } = useLocation();
	useEffect(() => {
		if (
			pathname === "/privacy" ||
			pathname === "/terms" ||
			pathname.startsWith("/docs/")
		) {
			// Immediate scroll for instant feedback, coordinated with content fade-in
			window.scrollTo({ top: 0, left: 0, behavior: "instant" });
		}
	}, [pathname]);
	return null;
}

function AppInner() {
	const [isLoaded, setIsLoaded] = useState(false);
	const location = useLocation();
	const shellClass = getShellClass(location.pathname);
	const isConceptRoute = location.pathname === "/concepts" || location.pathname.startsWith("/concepts/");

	useEffect(() => {
		// initialize GA
		initGA();

		document.documentElement.classList.add("preload");

		const timer = setTimeout(() => {
			setIsLoaded(true);
			document.documentElement.classList.remove("preload");
		}, 500);

		return () => clearTimeout(timer);
	}, []);

	return (
		<ThemeProvider>
			{/* Remount LanguageProvider when ?lang= changes to apply URL-driven language instantly */}
			<LanguageProvider
				key={new URLSearchParams(location.search).get("lang") ?? ""}
			>
				<Analytics />
				<ScrollTopOnDocs />
				<div className={shellClass}>
					{!isConceptRoute ? <DomainMigrationBanner /> : null}
					{!isConceptRoute ? <Navbar /> : null}
					<main
						className={`flex-1 transition-opacity duration-300 ${isLoaded ? "opacity-100" : "opacity-0"}`}
					>
						<Routes>
							<Route path="/" element={<Homepage />} />
							<Route path="/concepts" element={<HomepageConcept />} />
							<Route path="/privacy" element={<Privacy />} />
							<Route path="/terms" element={<Terms />} />
							{renderDocRoutes()}
						</Routes>
					</main>
					{!isConceptRoute ? <Footer /> : null}
				</div>
			</LanguageProvider>
		</ThemeProvider>
	);
}

function App() {
	return (
		<BrowserRouter>
			<AppInner />
		</BrowserRouter>
	);
}

export default App;
